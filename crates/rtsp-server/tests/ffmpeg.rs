//! Interoperability with real FFmpeg: reading H.264 and H.265 over TCP (and
//! with FFmpeg's default transport, which must fall back from UDP), and
//! publishing with `-f rtsp`. Skipped when the binaries are absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::tools::{copied_frames, encode, ffmpeg, ffprobe, framecrc_packets, run};
use common::{Clip, Factory, config, hub, start};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaKind, Want};

/// A hub with stream `cam` replaying `seconds` of encoded test pattern.
async fn clip_hub(codec: Codec) -> Option<Arc<oxrtc_core::Hub>> {
    let ffmpeg = ffmpeg()?;
    let data = encode(&ffmpeg, codec, 4).await;
    let clip = Clip::from_annex_b(codec, &data);
    assert!(clip.frames.len() >= 90, "{} frames", clip.frames.len());
    assert!(clip.frames.iter().filter(|f| f.1).count() >= 3, "keyframes");
    let factory = Arc::new(Factory::default());
    factory.clip("cam", clip);
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    Some(hub)
}

/// Reads `url` for three seconds with `-c copy`; returns the frames FFmpeg
/// copied and its error output.
async fn read(url: &str, transport: Option<&str>) -> (u64, String) {
    copied_frames(url, transport, 3).await
}

/// The counter itself, on what FFmpeg 6.1 to 8.0 write: a header of `#`
/// lines, then one line per packet.
#[test]
fn the_frame_counter_counts_framecrc_packet_lines() {
    let header = "#software: Lavf60.16.100\n#tb 0: 1/90000\n#media_type 0: video\n#codec_id 0: h264\n";
    let packets = "0,          0,          0,     3600,     5120, 0x6f7a2f3b\n\
                   0,       3600,       3600,     3600,      212, 0x3ad15f6d, F=0x0\n";
    assert_eq!(framecrc_packets(&format!("{header}{packets}")), 2);
    assert_eq!(framecrc_packets(header), 0, "a header alone");
    assert_eq!(framecrc_packets(""), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_h264_over_tcp() {
    let Some(hub) = clip_hub(Codec::H264).await else { return };
    let server = start(hub, config()).await;
    let (frames, errors) = read(&server.url("cam?video"), Some("tcp")).await;
    assert!(frames >= 60, "only {frames} frames");
    assert!(errors.trim().is_empty(), "ffmpeg reported: {errors}");
}

/// openIVR's analysis decoder and capture sidecar (R36): FFmpeg on
/// `?video&queue=live`, a query FFmpeg puts in every `SETUP` URL too. It reads
/// the picture, no error, and is listed as a live reader meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_the_analysis_url_as_a_live_reader() {
    let Some(hub) = clip_hub(Codec::H264).await else { return };
    let server = start(hub.clone(), config()).await;
    let url = server.url("cam?video&queue=live");
    let reading = tokio::spawn(async move { read(&url, Some("tcp")).await });
    let mut classes = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        classes = hub.get("cam").unwrap().snapshot().consumers.iter().map(|c| c.class.as_str()).collect();
        if !classes.is_empty() {
            break;
        }
    }
    let (frames, errors) = reading.await.unwrap();
    assert_eq!(classes, ["live"], "FFmpeg's reader is listed with the live class");
    assert!(frames >= 60, "only {frames} frames");
    assert!(errors.trim().is_empty(), "ffmpeg reported: {errors}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_falls_back_from_udp_to_tcp() {
    let Some(hub) = clip_hub(Codec::H264).await else { return };
    let server = start(hub, config()).await;
    let (frames, errors) = read(&server.url("cam?video"), None).await;
    assert!(frames >= 60, "only {frames} frames");
    // FFmpeg logs the refusal it falls back from, and nothing else.
    assert!(errors.contains("461"), "UDP was not tried: {errors}");
    let others: Vec<&str> = errors.lines().filter(|l| !l.contains("461")).collect();
    assert!(others.is_empty(), "ffmpeg reported: {others:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_h265_over_tcp() {
    let Some(hub) = clip_hub(Codec::H265).await else { return };
    let server = start(hub, config()).await;
    let (frames, errors) = read(&server.url("cam"), Some("tcp")).await;
    assert!(frames >= 60, "only {frames} frames");
    assert!(errors.trim().is_empty(), "ffmpeg reported: {errors}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffprobe_sees_the_stream() {
    let Some(hub) = clip_hub(Codec::H264).await else { return };
    let Some(ffprobe) = ffprobe() else { return };
    let server = start(hub, config()).await;
    let url = server.url("cam?video");
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
    assert_eq!(out.trim(), "h264,640,360", "stderr: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_publishes_into_a_registered_sink() {
    let Some(ffmpeg) = ffmpeg() else { return };
    let factory = Arc::new(Factory::default());
    let mut sinks = factory.handoff("pub", vec![(MediaKind::Video, Codec::H264)]);
    let hub = hub(factory);
    let stream = hub.upsert("pub", &["handoff:pub"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let sink = tokio::time::timeout(Duration::from_secs(5), sinks.recv()).await.unwrap().unwrap();
    let handle = server.publishers.register(sink);
    let url = handle.url();
    let publisher = tokio::spawn(async move {
        let args = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=25",
            "-t",
            "5",
            "-c:v",
            "libx264",
            "-g",
            "25",
            "-bf",
            "0",
            "-f",
            "rtsp",
            "-rtsp_transport",
            "tcp",
            &url,
        ];
        run(&ffmpeg, &args, Duration::from_secs(40)).await
    });
    // Meanwhile a reader reads the publication back through the server.
    let read_url = server.url("pub");
    let reader = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        read(&read_url, Some("tcp")).await
    });
    let (mut frames, mut keyframes, mut codec) = (0u32, 0u32, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while frames < 100 {
        let Ok(event) = tokio::time::timeout_at(deadline, sub.recv()).await else { break };
        match event.unwrap() {
            Event::Tracks(tracks) => codec = Some(tracks[0].codec),
            Event::Frame { frame, .. } => {
                frames += 1;
                keyframes += u32::from(frame.keyframe);
            }
            Event::Discontinuity => {}
        }
    }
    // The handoff source declares its tracks itself, so Tracks proves nothing
    // about FFmpeg: its own outcome is what explains a run with no frames.
    let (ok, _, err) = publisher.await.unwrap();
    assert_eq!(codec, Some(Codec::H264));
    assert!(frames >= 100, "{frames} frames; publisher ok={ok}, stderr: {err}");
    assert!(keyframes >= 2, "{keyframes} keyframes");
    assert!(ok, "publisher failed: {err}");
    let (read_frames, read_errors) = reader.await.unwrap();
    assert!(read_frames >= 25, "read back {read_frames} frames");
    assert!(read_errors.trim().is_empty(), "reader reported: {read_errors}");
    drop(handle);
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_pcma_audio_alone() {
    let Some(ffmpeg) = ffmpeg() else { return };
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::synthetic());
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub, config()).await;
    let url = server.url("cam?audio=pcma");
    let args = [
        "-hide_banner",
        "-nostats",
        "-loglevel",
        "error",
        "-rtsp_transport",
        "tcp",
        "-i",
        &url,
        "-t",
        "2",
        "-c",
        "copy",
        "-f",
        "null",
        "-progress",
        "pipe:1",
        "-",
    ];
    let (ok, out, err) = run(&ffmpeg, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffmpeg failed: {err}");
    assert!(err.trim().is_empty(), "ffmpeg reported: {err}");
    let micros: u64 = out
        .lines()
        .filter_map(|l| l.strip_prefix("out_time_us="))
        .filter_map(|v| v.trim().parse().ok())
        .next_back()
        .unwrap_or(0);
    assert!(micros >= 1_500_000, "only {micros} us of audio");
}
