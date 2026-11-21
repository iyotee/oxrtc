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
use oxrtc_core::{Codec, Frame, MediaKind, TrackInfo};

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

/// What ffprobe reads of `url` without guessing the rate, and how long it took.
async fn rate_unguessed(url: &str) -> (String, Duration) {
    let ffprobe = ffprobe().unwrap();
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
        url,
    ];
    let started = std::time::Instant::now();
    let (ok, out, err) = run(&ffprobe, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffprobe failed: {err}");
    (out.trim().to_owned(), started.elapsed())
}

/// The installed ffprobe's major release.
async fn ffprobe_major() -> Option<u32> {
    let (_, out, _) = run(&ffprobe()?, &["-version"], Duration::from_secs(10)).await;
    let version = out.lines().next()?.strip_prefix("ffprobe version ")?.split_whitespace().next()?.to_owned();
    if version.starts_with("N-") {
        return Some(u32::MAX);
    }
    version.trim_start_matches('n').chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// A stream `cam` of one JPEG picture dated `steps` ticks of 90 kHz after one
/// another, over and over, each sent when its date comes: a camera's own
/// clock, as the test chooses it.
fn dated_webcam(steps: &'static [i64]) -> Arc<oxrtc_core::Hub> {
    let factory = Arc::new(Factory::default());
    let mut sinks = factory.handoff("cam", vec![(MediaKind::Video, Codec::Jpeg)]);
    tokio::spawn(async move {
        // One sink a run of the source.
        while let Some(sink) = sinks.recv().await {
            if sink.set_tracks(vec![TrackInfo::video(Codec::Jpeg)]).is_err() {
                continue;
            }
            let mut date = 0;
            for step in steps.iter().cycle() {
                if sink.publish(Frame::new(0, date, true, Bytes::copy_from_slice(PIL_420))).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_micros(u64::try_from(*step).unwrap() * 100 / 9)).await;
                date += step;
            }
        }
    });
    let hub = hub(factory);
    hub.upsert("cam", &["handoff:cam"]).unwrap();
    hub
}

/// 25 pictures a second on average, each dated when it came over a network
/// that kept some back and let others through together: no step they share.
const AS_IT_ARRIVES: &[i64] = &[2100, 5300, 3600, 2500, 4700, 3100, 4100, 3300, 3700];
/// The owner's ESP32, as its dates read through his relay on 2026-11-19
/// (ffprobe's `pts_time`, in ticks): 25 slots a second, one in five filled,
/// each date within a millisecond of its slot.
const ONE_SLOT_IN_FIVE: &[i64] = &[21591, 14400, 21609, 14373, 18018, 28773, 46773, 17991, 18063, 14319, 14391, 18000];
/// A webcam of two pictures a second.
const TWO_A_SECOND: &[i64] = &[45_011, 44_987, 45_003, 44_999];
/// 30 pictures a second, each dated as it arrived up to 8 ms off its slot:
/// an uneven minute of the owner's webcam, on which FFmpeg's guess finds no
/// rate (its median gap is 37.8 ms).
const UNEVEN_THIRTY: &[i64] =
    &[3700, 1800, 4150, 1700, 3950, 2000, 4200, 2200, 4020, 1680, 3600, 1880, 4320, 2000, 3400];
/// The owner's 30-a-second webcam on an even minute, dated as it arrives,
/// through his relay on the evening of 2026-11-19 (ticks): 24 to 40.5 ms
/// between two pictures. FFmpeg's guess read `30/1` from these dates.
const EVEN_MINUTE: &[i64] = &[2943, 2934, 3366, 2619, 3384, 2961, 2556, 3096, 2925, 3177, 2817, 3222, 3645, 2160, 3159];

/// What the relay says of a Motion JPEG camera's rate (`a=framerate`, D36) is
/// what FFmpeg's own guess read, and FFmpeg 8 told not to guess reads it at
/// once, where its guess read twenty pictures (10 s at two a second); 6.1 and
/// 7.1 do not read the attribute (the transcodes keep their guess there,
/// `crates/ffmpeg` `version.rs`):
/// - a webcam of two pictures a second, dated as they arrive: its pace;
/// - one of 25 a second, and one of 30, dated as they arrive on an uneven
///   minute: their rate, the mean of their gaps (the owner, 2026-11-20:
///   always give x264 the camera's rate, D36), where FFmpeg's guess finds
///   none in such dates and the relay said nothing until then;
/// - one of 30 a second on an even minute (the owner's, a tenth of a slot
///   off at most on average): 30, which FFmpeg's guess read too - told no
///   rate, its transcode carried 60 % fewer bits than 0.2.0's that evening;
/// - a camera with a clock of its own that stamps 25 slots a second and fills
///   one in five (the owner's ESP32): 25, the grid, which is what FFmpeg's
///   guess reads - announced at its pace, its transcode carried twice the
///   bits (W4 review).
#[tokio::test(flavor = "multi_thread")]
async fn a_cameras_rate_is_announced_to_ffmpeg_as_its_own_guess_read_it() {
    let reads_it = ffprobe_major().await.is_some_and(|m| m >= 8);
    // The rate of dates on no grid is read from the gaps of the moment: within
    // 5 % of 25 for the nine gaps of AS_IT_ARRIVES that a window of fifteen
    // holds from wherever it starts (24.2 to 25.5).
    for (what, steps, expected, within) in [
        ("two a second", TWO_A_SECOND, Some(2.0), 0.05),
        ("25 a second as they arrive", AS_IT_ARRIVES, Some(25.0), 1.3),
        ("30 a second on an uneven minute", UNEVEN_THIRTY, Some(30.0), 0.05),
        ("30 a second on an even minute", EVEN_MINUTE, Some(30.0), 0.05),
        ("one slot in five of 25 a second", ONE_SLOT_IN_FIVE, Some(25.0), 0.05),
    ] {
        let hub = dated_webcam(steps);
        let stream = hub.get("cam").unwrap();
        // The source running for a while, as the NVR's recorder keeps it,
        // until its pace is read from a full window of gaps.
        let mut held = stream.subscribe(&oxrtc_core::Want::all(), oxrtc_core::ConsumerTag::new("test")).await.unwrap();
        let known = tokio::time::timeout(Duration::from_secs(15), async {
            let mut pictures = 0;
            while pictures < 16 {
                if let Ok(oxrtc_core::Event::Frame { .. }) = held.recv().await {
                    pictures += 1;
                }
            }
        });
        known.await.expect("the source sent no sixteen pictures");
        assert!(held.picture_interval().is_some(), "{what}");
        let server = start(hub, config()).await;
        let mut client = common::Client::connect(server.addr).await;
        let sdp =
            client.request("DESCRIBE", &server.url("cam?video"), &["Accept: application/sdp"], "").await.unwrap().body;
        let said = sdp.lines().find_map(|line| line.strip_prefix("a=framerate:")).map(|r| r.trim().parse::<f64>());
        match (expected, said) {
            (Some(want), Some(Ok(said))) => assert!((said - want).abs() < within, "{what}: {sdp}"),
            (None, None) => {}
            _ => panic!("{what}: {sdp}"),
        }
        if !reads_it {
            continue;
        }
        let (rate, took) = rate_unguessed(&server.url("cam?video")).await;
        let read = rate.split_once('/').and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?));
        match expected {
            Some(want) => assert!(read.is_some_and(|r| (r - want).abs() < within), "{what}: {rate}"),
            None => assert_eq!(rate, "0/0", "{what}"),
        }
        assert!(took < Duration::from_secs(5), "{what}: {took:?}");
        drop(held);
    }
}

/// FFmpeg's own guess, which the announcement replaces, on the same camera:
/// it reads the grid of the dates (25 a second), not the pace of the pictures
/// (5 a second). The reason the relay says the grid.
#[tokio::test(flavor = "multi_thread")]
async fn ffmpegs_own_guess_of_a_camera_that_skips_slots_is_its_grid() {
    let Some(ffprobe) = ffprobe() else { return };
    let server = start(dated_webcam(ONE_SLOT_IN_FIVE), config()).await;
    let url = server.url("cam?video");
    let args =
        ["-v", "error", "-rtsp_transport", "tcp", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0", &url];
    let (ok, out, err) = run(&ffprobe, &args, Duration::from_secs(40)).await;
    assert!(ok, "ffprobe failed: {err}");
    assert_eq!(out.trim(), "25/1", "stderr: {err}");
}
