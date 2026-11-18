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
    let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
    let mut sender = Sender::new(outputs, &tracks, Playout::default());
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
