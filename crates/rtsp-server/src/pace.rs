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
//!
//! The wait is the reader's to afford. That FFmpeg gives up on an input that
//! has not answered within its socket timeout - 5 s unless the source says
//! `#timeout=`, which is in the URL too - and the source's own start is part
//! of what it waited: a camera that takes 2.5 s to answer and then shows a
//! picture every 2 s had its answer held to 6.5 s, FFmpeg left at 5 with
//! "Invalid data found when processing input", and the viewer was refused
//! where an answer with no rate at 2.5 s would have served it. The answer is
//! never held past the reader's timeout less a second ([`pace_wait`]).

use std::time::Duration;

use oxrtc_core::Subscription;
use tokio::time::Instant;

/// Longest wait for the pace of a transcode's pictures: three gaps of a
/// source of one picture a second, and a second for their jitter. A slower
/// source is answered without a rate after it (FFmpeg's guess needed twenty
/// of its pictures, more than the transcode's start timeout allows).
pub(crate) const PACE_WAIT: Duration = Duration::from_secs(4);

/// How long one of this server's FFmpeg waits for an answer when its source
/// says no `#timeout=`: the `timeout` template's default (`crates/ffmpeg`,
/// as go2rtc's).
const READER_TIMEOUT: Duration = Duration::from_secs(5);

/// What is left of the reader's timeout once it is answered: the answer's
/// way back, and a loaded machine.
const READER_MARGIN: Duration = Duration::from_secs(1);

/// The longest `#timeout=` believed (`oxrtc_core::spec`: an hour).
const LONGEST_TIMEOUT_SECS: u64 = 3600;

/// How often the wait looks at the source.
const PACE_POLL: Duration = Duration::from_millis(20);

/// Whether the reader of `query` says it is a transcode of this server.
pub(crate) fn reads_for_a_transcode(query: &str) -> bool {
    query.split('&').any(|pair| pair.strip_prefix("source=").is_some_and(|source| source.starts_with("ffmpeg:")))
}

/// How long the reader of `query`, a transcode whose request came `waited`
/// ago, may still be held for the pace of its pictures: [`PACE_WAIT`], cut
/// to what its FFmpeg's timeout leaves, a second kept back. Nothing once that
/// is spent: the source's start took it all.
pub(crate) fn pace_wait(query: &str, waited: Duration) -> Duration {
    PACE_WAIT.min(reader_timeout(query).saturating_sub(READER_MARGIN).saturating_sub(waited))
}

/// The socket timeout of the FFmpeg that reads `query`: the `#timeout=N` of
/// the source it says it is (`source=ffmpeg:cam%23video%3Dh264%23timeout%3D20`,
/// the source's text as the driver escapes it), 5 s when it says none. A
/// value that is no number of seconds counts as none: the short side.
fn reader_timeout(query: &str) -> Duration {
    const KEY: &str = "%23timeout%3d";
    let source = query.split('&').find_map(|pair| pair.strip_prefix("source=")).unwrap_or_default();
    let given = source.to_ascii_lowercase().rfind(KEY).and_then(|at| {
        let digits: String = source.get(at + KEY.len()..)?.chars().take_while(char::is_ascii_digit).take(5).collect();
        digits.parse::<u64>().ok().filter(|secs| (1..=LONGEST_TIMEOUT_SECS).contains(secs))
    });
    given.map_or(READER_TIMEOUT, Duration::from_secs)
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

    #[test]
    fn the_wait_is_cut_to_what_the_readers_timeout_leaves() {
        let secs = Duration::from_secs;
        let plain = "video&source=ffmpeg:cam%23video%3Dh264&queue=live";
        let patient = "video&source=ffmpeg:camera_4_src%23video%3Dh264%23width%3D1920%23timeout%3D20&queue=live";
        // FFmpeg's 5 s, a second kept back: a camera known at once is waited
        // for four seconds, one that took 2.5 s to answer 1.5 s more, one
        // that took 4 s or longer not at all.
        assert_eq!(pace_wait(plain, Duration::ZERO), secs(4));
        assert_eq!(pace_wait(plain, Duration::from_millis(2500)), Duration::from_millis(1500));
        assert_eq!(pace_wait(plain, secs(4)), Duration::ZERO);
        assert_eq!(pace_wait(plain, secs(28)), Duration::ZERO);
        // A source that says `#timeout=20` (openIVR's) can afford the whole wait
        // after a slow start, until its own 19 s are spent.
        assert_eq!(pace_wait(patient, Duration::from_millis(2500)), secs(4));
        assert_eq!(pace_wait(patient, secs(17)), secs(2));
        assert_eq!(pace_wait(patient, secs(25)), Duration::ZERO);
        for (query, timeout) in [
            ("source=ffmpeg:cam%23timeout%3D1", 1),
            ("source=ffmpeg:cam%23timeout%3d3600%23video%3Dh264", 3600),
            // The last one a source says is the one its FFmpeg was given.
            ("source=ffmpeg:cam%23timeout%3D9%23timeout%3D7", 7),
            // No number of seconds, or none at all: FFmpeg's default.
            ("source=ffmpeg:cam%23timeout%3D0", 5),
            ("source=ffmpeg:cam%23timeout%3D99999999999999999999", 5),
            ("source=ffmpeg:cam%23timeout%3Dsoon", 5),
            ("source=ffmpeg:cam%23timeout%3D", 5),
            ("video&timeout=20&source=ffmpeg:cam", 5),
            ("video", 5),
            ("", 5),
        ] {
            assert_eq!(reader_timeout(query), secs(timeout), "{query}");
        }
    }
}
