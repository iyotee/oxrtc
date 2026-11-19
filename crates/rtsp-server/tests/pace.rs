//! The pace of a coded camera's pictures in `DESCRIBE` (`a=framerate`), and
//! the one reader whose answer waits for it: one of the server's own
//! transcodes, whose FFmpeg encodes at the rate the answer says (W4 review,
//! D36). The owner's H.265 cameras say no rate in their parameter sets: read
//! without FFmpeg's guess and without this line, their transcode was encoded
//! as 90000 pictures a second, at half the bits.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::tools::{encode, ffmpeg, ffprobe, run};
use common::{Client, Clip, Factory, Running, config, hub, start};
use oxrtc_core::{Codec, Frame, MediaKind};

/// What the `ffmpeg:` driver writes in the URL of a stream it reads by name.
const TRANSCODE: &str = "cam?video&source=ffmpeg:cam%23video%3Dh264&queue=live";

/// A server whose stream `cam` replays `clip`, nothing reading it yet.
async fn camera(clip: Clip) -> Running {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", clip);
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    start(hub, config()).await
}

/// The `DESCRIBE` of `path` on a connection of its own, and how long it took.
async fn described(server: &Running, path: &str) -> (String, Duration) {
    let mut client = Client::connect(server.addr).await;
    let started = Instant::now();
    let answer = client.request("DESCRIBE", &server.url(path), &["Accept: application/sdp"], "").await.unwrap();
    (answer.body, started.elapsed())
}

fn rate_said(sdp: &str) -> Option<f64> {
    sdp.lines().find_map(|line| line.strip_prefix("a=framerate:")).and_then(|rate| rate.trim().parse().ok())
}

/// Ten pictures a second of tiny H.264, as a camera nothing reads yet.
fn ten_a_second() -> Clip {
    Clip { fps: 10, ..Clip::synthetic() }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_transcode_of_a_camera_is_answered_once_its_pace_is_known() {
    let server = camera(ten_a_second()).await;
    let (sdp, took) = described(&server, TRANSCODE).await;
    let said = rate_said(&sdp).unwrap_or_else(|| panic!("no rate said to the transcode: {sdp}"));
    assert!((said - 10.0).abs() < 0.05, "{sdp}");
    // Three gaps of 100 ms after the first picture, not FFmpeg's twenty.
    assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(2), "{took:?}");
    // The pace is known from then on: the next transcode does not wait.
    let (sdp, took) = described(&server, TRANSCODE).await;
    assert!(rate_said(&sdp).is_some_and(|r| (r - 10.0).abs() < 0.05), "{sdp}");
    assert!(took < Duration::from_millis(200), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_is_no_transcode_is_answered_at_once() {
    let server = camera(ten_a_second()).await;
    let (sdp, took) = described(&server, "cam?video").await;
    assert!(sdp.contains("m=video") && rate_said(&sdp).is_none(), "the pace is not known yet: {sdp}");
    assert!(took < Duration::from_millis(250), "{took:?}");
    // A transcode that asks for the sound alone waits for no picture.
    let server = camera(ten_a_second()).await;
    let (sdp, took) = described(&server, "cam?audio&source=ffmpeg:cam%23audio%3Dopus").await;
    assert!(sdp.contains("m=audio") && !sdp.contains("m=video"), "{sdp}");
    assert!(took < Duration::from_millis(250), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_of_a_camera_that_shows_no_pace_is_answered_after_the_wait() {
    // One picture, then nothing: the answer is held four seconds, no longer.
    let factory = Arc::new(Factory::default());
    let mut sinks = factory.handoff("cam", vec![(MediaKind::Video, Codec::H264)]);
    let hub = hub(factory);
    hub.upsert("cam", &["handoff:cam"]).unwrap();
    let server = start(hub, config()).await;
    let publisher = tokio::spawn(async move {
        let sink = sinks.recv().await.unwrap();
        sink.set_tracks(Clip::synthetic().tracks[..1].to_vec()).unwrap();
        sink.publish(Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]))).unwrap();
        sink
    });
    let (sdp, took) = described(&server, TRANSCODE).await;
    assert!(sdp.contains("m=video") && rate_said(&sdp).is_none(), "{sdp}");
    assert!(took >= Duration::from_secs(4) && took < Duration::from_secs(6), "{took:?}");
    drop(publisher.await.unwrap());
}

/// On the real client: FFmpeg 8 told not to guess (`-fpsprobesize 0`, as the
/// relay's transcodes are) reads the pace the relay says for an H.265 camera,
/// here ten pictures a second from a clip whose own headers say twenty-five.
/// FFmpeg 6.1 and 7.1 do not read the attribute, and keep their guess
/// (`crates/ffmpeg` `version.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_the_pace_of_a_coded_camera_without_guessing() {
    let (Some(ffmpeg), Some(ffprobe)) = (ffmpeg(), ffprobe()) else { return };
    let clip = Clip { fps: 10, ..Clip::from_annex_b(Codec::H265, &encode(&ffmpeg, Codec::H265, 2).await) };
    let server = camera(clip).await;
    let (sdp, _) = described(&server, TRANSCODE).await;
    assert!(rate_said(&sdp).is_some_and(|r| (r - 10.0).abs() < 0.05), "{sdp}");
    let (_, version, _) = run(&ffprobe, &["-version"], Duration::from_secs(10)).await;
    let release = version.lines().next().and_then(|l| l.strip_prefix("ffprobe version ")).unwrap_or_default();
    let major: Option<u32> =
        release.trim_start_matches('n').chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok();
    if !(release.starts_with("N-") || major.is_some_and(|m| m >= 8)) {
        return;
    }
    let url = server.url(TRANSCODE);
    let args = [
        "-v",
        "error",
        "-rtsp_transport",
        "tcp",
        "-fpsprobesize",
        "0",
        "-show_entries",
        "stream=avg_frame_rate",
        "-of",
        "csv=p=0",
        url.as_str(),
    ];
    let (ok, out, err) = run(&ffprobe, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffprobe failed: {err}");
    let read = out.trim().split_once('/').and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?));
    assert!(read.is_some_and(|r| (r - 10.0).abs() < 0.05), "FFmpeg read {out:?} where the relay said ten a second");
}
