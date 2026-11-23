//! The one path every egress that knows its reader takes for a camera's
//! H.265 (D41, D44): the camera's picture when the reader takes its profile,
//! tier and level, else the stream's H.264, the camera held until then; the
//! camera's H.265 anyway when the stream has nothing else; a reader that says
//! nothing (`stream.mp4`) is sent what comes. openIVR's layout, fake sources.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::h265_fallback::{self, without_h265};
use oxrtc_core::h265_receiver::HevcReceiver;
use oxrtc_core::{Codec, CodecParams, ConsumerTag, MediaKind, MediaWant, Stream, TrackInfo, Want};

/// libx265's SPS of a Main stream at level 4 (Main 10 compatible).
const MAIN_L4: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";
/// libx265's Main 10 at level 2.1.
const MAIN10_L21: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

fn hex(s: &str) -> Bytes {
    Bytes::from((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>())
}

fn camera_script(sps: &str) -> Script {
    let params = CodecParams::H265 {
        vps: Bytes::from_static(&[0x40, 0x01]),
        sps: hex(sps),
        pps: Bytes::from_static(&[0x44, 0x01]),
    };
    Script {
        tracks: vec![TrackInfo::video(Codec::H265).with_params(params)],
        ..Script::video(Codec::H265, Script::gop(50))
    }
}

/// openIVR's `camera_N`: the camera, then (with `transcode`) its H.264.
fn layout(sps: &str, transcode: bool) -> (Arc<Stream>, Arc<common::Fake>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("camera", camera_script(sps));
    let h264 = factory.add("transcode", Script::video(Codec::H264, Script::gop(50)));
    let hub = hub_with(factory, limits());
    let sources: &[&str] = if transcode { &["fake:camera", "fake:transcode"] } else { &["fake:camera"] };
    (hub.upsert("camera_1", sources).unwrap(), h264)
}

fn video(want: &[Codec]) -> Want {
    Want { video: MediaWant::OneOf(want.to_vec()), audio: MediaWant::Skip }
}

fn go2rtc_player() -> Vec<HevcReceiver> {
    vec![HevcReceiver::from_codec_string("hvc1.1.6.L153.B0").unwrap()]
}

fn codec_of(taken: &h265_fallback::Taken) -> Codec {
    taken.tracks.iter().find(|t| t.kind == MediaKind::Video).unwrap().codec
}

#[tokio::test]
async fn a_camera_the_reader_takes_is_sent_and_the_transcode_never_starts() {
    let (stream, transcode) = layout(MAIN_L4, true);
    let want = video(&[Codec::H265, Codec::H264]);
    let taken =
        h265_fallback::subscribe(&stream, &want, Some(&go2rtc_player()), ConsumerTag::new("mse")).await.unwrap();
    assert_eq!((codec_of(&taken), taken.passed_over), (Codec::H265, None));
    assert_eq!(transcode.runs(), 0);
}

#[tokio::test]
async fn a_main10_camera_to_a_main_reader_gets_the_transcode() {
    let (stream, transcode) = layout(MAIN10_L21, true);
    let want = video(&[Codec::H265, Codec::H264]);
    let taken =
        h265_fallback::subscribe(&stream, &want, Some(&go2rtc_player()), ConsumerTag::new("mse")).await.unwrap();
    assert_eq!(codec_of(&taken), Codec::H264);
    assert_eq!(taken.passed_over.map(|c| c.profile), Some(2));
    assert!(transcode.runs() >= 1);
    // Only the transcode is held: the camera's demand was let go.
    drop(taken);
    let main10 = [HevcReceiver::from_codec_string("hvc1.2.4.L153.B0").unwrap()];
    let taken = h265_fallback::subscribe(&stream, &want, Some(&main10), ConsumerTag::new("mse")).await.unwrap();
    assert_eq!((codec_of(&taken), taken.passed_over), (Codec::H265, None), "a Main 10 entry takes it");
}

#[tokio::test]
async fn with_nothing_else_the_camera_is_sent_anyway_and_a_reader_that_says_nothing_is_not_checked() {
    let (stream, _) = layout(MAIN10_L21, false);
    let want = video(&[Codec::H265, Codec::H264]);
    let taken =
        h265_fallback::subscribe(&stream, &want, Some(&go2rtc_player()), ConsumerTag::new("mse")).await.unwrap();
    assert_eq!((codec_of(&taken), taken.passed_over), (Codec::H265, None));
    drop(taken);
    let (stream, transcode) = layout(MAIN10_L21, true);
    let taken = h265_fallback::subscribe(&stream, &want, None, ConsumerTag::new("mp4")).await.unwrap();
    assert_eq!((codec_of(&taken), taken.passed_over), (Codec::H265, None));
    assert_eq!(transcode.runs(), 0);
}

#[test]
fn the_fallback_wish_is_the_same_without_h265() {
    assert_eq!(without_h265(&video(&[Codec::H265, Codec::H264])), video(&[Codec::H264]));
    assert_eq!(without_h265(&video(&[Codec::H265])).video, MediaWant::Skip);
    assert_eq!(without_h265(&Want::all()), Want::all());
}
