//! Motion JPEG over RTP (RFC 2435) read by real FFmpeg: the picture it
//! decodes from the RTP packets is the one it decodes from the file, bit for
//! bit, for every chroma layout and restart interval the server carries.
//! Skipped when FFmpeg is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::tools::{copied_frames, ffmpeg, ffprobe, run};
use common::{Clip, Factory, config, hub, start};
use oxrtc_core::{Codec, TrackInfo};

const PIL_420: &[u8] = include_bytes!("data/pil420.jpg");
const PIL_422: &[u8] = include_bytes!("data/pil422.jpg");
const PIL_420_RESTART: &[u8] = include_bytes!("data/pil420_restart.jpg");
/// Coded with Huffman tables fitted to the picture: FFmpeg's encoder, then
/// libjpeg's `optimize_coding` (4:2:2; 4:2:0 with restart markers; noise).
const FFMPEG_OPTIMAL: &[u8] = include_bytes!("data/optimal.jpg");
const OPTIMAL_422: &[u8] = include_bytes!("data/optimal422.jpg");
const OPTIMAL_RESTART: &[u8] = include_bytes!("data/optimal420_restart.jpg");
const OPTIMAL_NOISE: &[u8] = include_bytes!("data/optimal_noise.jpg");

/// A stream `cam` replaying one JPEG file at 25 pictures a second.
fn webcam(file: &[u8]) -> Arc<oxrtc_core::Hub> {
    let factory = Arc::new(Factory::default());
    factory.clip(
        "cam",
        Clip {
            tracks: vec![TrackInfo::video(Codec::Jpeg)],
            frames: vec![(0, true, Bytes::copy_from_slice(file))],
            fps: 25,
        },
    );
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    hub
}

/// The MD5 FFmpeg prints for the first decoded picture of `input`.
async fn first_picture(input: &str) -> String {
    let ffmpeg = ffmpeg().unwrap();
    let mut args = vec!["-hide_banner", "-loglevel", "error"];
    if input.starts_with("rtsp://") {
        args.extend(["-rtsp_transport", "tcp"]);
    }
    args.extend(["-i", input, "-frames:v", "1", "-f", "md5", "-"]);
    let (ok, out, err) = run(&ffmpeg, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffmpeg failed on {input}: {err}");
    assert!(err.trim().is_empty(), "ffmpeg reported on {input}: {err}");
    out.trim().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_picture_ffmpeg_decodes_from_rtp_is_the_one_in_the_file() {
    if ffmpeg().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (name, file) in [
        ("4:2:0", PIL_420),
        ("4:2:2", PIL_422),
        ("4:2:0 with restarts", PIL_420_RESTART),
        ("FFmpeg's own Huffman tables", FFMPEG_OPTIMAL),
        ("4:2:2 with its own Huffman tables", OPTIMAL_422),
        ("4:2:0 with restarts and its own Huffman tables", OPTIMAL_RESTART),
        ("noise with its own Huffman tables", OPTIMAL_NOISE),
    ] {
        let path = dir.path().join("frame.jpg");
        std::fs::write(&path, file).unwrap();
        let expected = first_picture(path.to_str().unwrap()).await;
        assert!(expected.starts_with("MD5="), "{expected}");
        let server = start(webcam(file), config()).await;
        let received = first_picture(&server.url("cam?video")).await;
        assert_eq!(received, expected, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_a_webcam_stream_at_its_pace_and_ffprobe_sees_its_size() {
    if ffmpeg().is_none() {
        return;
    }
    let server = start(webcam(PIL_420), config()).await;
    let url = server.url("cam");
    let (frames, err) = copied_frames(&url, Some("tcp"), 3).await;
    assert!(err.trim().is_empty(), "ffmpeg reported: {err}");
    assert!((60..=90).contains(&frames), "{frames} pictures in 3 s at 25 a second");
    let Some(ffprobe) = ffprobe() else { return };
    let args = [
        "-v",
        "error",
        "-rtsp_transport",
        "tcp",
        "-show_entries",
        "stream=codec_name,width,height",
        "-of",
        "csv=p=0",
        &url,
    ];
    let (ok, out, err) = run(&ffprobe, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffprobe failed: {err}");
    assert_eq!(out.trim(), "mjpeg,64,48", "stderr: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_picture_the_format_cannot_carry_makes_a_gap_and_the_next_one_goes() {
    if ffmpeg().is_none() {
        return;
    }
    let refused = include_bytes!("data/yuvj422p.jpg");
    let factory = Arc::new(Factory::default());
    // Three refused pictures for every one that goes.
    let frames = vec![
        (0, true, Bytes::from_static(refused)),
        (0, true, Bytes::from_static(refused)),
        (0, true, Bytes::from_static(refused)),
        (0, true, Bytes::copy_from_slice(PIL_420)),
    ];
    factory.clip("cam", Clip { tracks: vec![TrackInfo::video(Codec::Jpeg)], frames, fps: 25 });
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub, config()).await;
    let (frames, _) = copied_frames(&server.url("cam"), Some("tcp"), 3).await;
    assert!((12..=30).contains(&frames), "{frames} pictures, one in four of 75");
}
