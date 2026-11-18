//! The sender's own decisions, without a peer.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::Codec;
use str0m::format::{Codec as RtcCodec, CodecSpec, FormatParams, PayloadParams};
use str0m::media::{Frequency, Mid};

use super::*;

/// A negotiated sound line taking Opus.
fn sound_line() -> Output {
    let spec = CodecSpec {
        codec: RtcCodec::Opus,
        clock_rate: Frequency::FORTY_EIGHT_KHZ,
        channels: Some(2),
        format: FormatParams::default(),
    };
    Output { mid: Mid::from("1"), kind: MediaKind::Audio, params: vec![PayloadParams::new(111.into(), None, spec)] }
}

/// A camera's picture and sound, the session connected `since` ago, and a
/// frame of each track that entered `age` ago.
fn connected(outputs: Vec<Output>, since: Duration, age: Duration) -> (Sender, Frame, Frame) {
    connected_with(outputs, since, age, Playout::default())
}

fn connected_with(outputs: Vec<Output>, since: Duration, age: Duration, playout: Playout) -> (Sender, Frame, Frame) {
    let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
    let mut sender = Sender::new(outputs, &tracks, playout);
    let now = tokio::time::Instant::now();
    sender.connected = Some(now - since);
    let frame = |track| Frame { track, pts: 0, keyframe: true, received: now - age, data: Bytes::from_static(b"x") };
    (sender, frame(0), frame(1))
}

const SECOND: Duration = Duration::from_secs(1);
const LONGER: Duration = Duration::from_secs(2);
const SHORTER: Duration = Duration::from_millis(500);

#[test]
fn the_sound_from_before_the_connection_is_stale_and_the_picture_is_not() {
    let (sender, picture, sound) = connected(vec![sound_line()], SECOND, LONGER);
    assert!(sender.stale(1, &sound), "the sound that entered before the connection");
    assert!(!sender.stale(0, &picture), "the picture that waited is sent");
    let (sender, picture, sound) = connected(vec![sound_line()], SECOND, SHORTER);
    assert!(!sender.stale(1, &sound), "the sound that entered since the connection");
    assert!(!sender.stale(0, &picture));
}

#[test]
fn nothing_is_stale_before_the_connection() {
    let (mut sender, picture, sound) = connected(vec![sound_line()], SECOND, LONGER);
    sender.connected = None;
    assert!(!sender.stale(1, &sound) && !sender.stale(0, &picture));
}

/// A viewer that asked for the picture alone: the camera's sound has no
/// media line, nothing of it is held back, and the session's last log line
/// does not count sound it never had (`stale_sound`).
#[test]
fn a_sound_track_the_session_does_not_send_is_not_counted() {
    let (sender, _, sound) = connected(Vec::new(), SECOND, LONGER);
    assert!(sender.tracks[1].target.is_none(), "no media line takes the sound");
    assert!(!sender.stale(1, &sound));
    assert!(!sender.stale(7, &sound), "a track the session does not know");
}

/// The session's last log line says how long the sound was held on average
/// (`sound_hold_mean_ms`), beside the longest: the mean is what the sound's
/// path costs, and in lip sync the picture's too (W1b review: the longest
/// read the 40 ms bound in every L01 run, the mean 38 ms).
#[test]
fn the_mean_hold_is_the_holds_of_the_sound_placed_over_their_count() {
    let paced = Playout { pace_sound: true, ..Playout::default() };
    let (mut sender, _, _) = connected_with(vec![sound_line()], SECOND, SHORTER, paced);
    assert_eq!(sender.sound_hold_mean(), Duration::ZERO, "nothing placed");
    let start = tokio::time::Instant::now();
    // Pairs of 20 ms frames every 40 ms: the first frame waits for nothing,
    // every later one is held 20 ms (the pair's second frame that came early).
    for n in 0..20u32 {
        let frame = Arc::new(Frame {
            track: 1,
            pts: i64::from(n) * 960,
            keyframe: false,
            received: start + Duration::from_millis(u64::from(n / 2) * 40),
            data: Bytes::from_static(b"x"),
        });
        assert!(sender.hold(1, &frame), "a sound track paced");
    }
    assert_eq!(sender.sound_hold, Duration::from_millis(20));
    assert_eq!(sender.sound_hold_mean(), Duration::from_millis(19), "19 holds of 20 ms and one of 0, over 20");
}

/// Unpaced (the default), a sound frame on time is the caller's to write at
/// once, and one a stall kept back beyond the bound is taken and counted.
#[test]
fn unpaced_the_bound_still_judges_the_sound() {
    let (mut sender, _, _) = connected(vec![sound_line()], SECOND, SHORTER);
    let start = tokio::time::Instant::now();
    let frame = |n: i64, at_ms: u64| {
        Arc::new(Frame {
            track: 1,
            pts: n * 960,
            keyframe: false,
            received: start + Duration::from_millis(at_ms),
            data: Bytes::from_static(b"x"),
        })
    };
    for n in 0..50 {
        assert!(!sender.hold(1, &frame(n, u64::try_from(n).unwrap() * 20)), "frame {n} on time: written now");
    }
    // Frame 50 was due at 1000 ms and comes at 1600: 600 ms behind, beyond 400.
    assert!(sender.hold(1, &frame(50, 1600)), "late: taken, not written");
    assert_eq!(sender.late_sound, 1);
    assert_eq!(sender.held.len(), 0, "nothing waits");
    assert_eq!(sender.sound_hold_mean(), Duration::ZERO, "nothing held");
    // Without a bound and unpaced, the session has no clock at all.
    let off = Playout { sound_late_max: None, ..Playout::default() };
    let (mut sender, _, _) = connected_with(vec![sound_line()], SECOND, SHORTER, off);
    assert!(!sender.hold(1, &frame(50, 1600)));
}
