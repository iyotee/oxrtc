//! openIVR's analysis decoders on the binary, with the real readers (R36).
//!
//! `analysis_url()` is `rtsp://127.0.0.1:P/camera_N_sub?video&queue=live` (or
//! `camera_N?video&queue=live` when the camera has no substream), read by
//! OpenCV's FFmpeg; the capture sidecar's `capture_url()` is
//! `camera_N?video&queue=live`, read by an FFmpeg child and probed by
//! `ffprobe` first. Checked here:
//!
//! - go2rtc, the reference, answers the very URL (it ignores `queue`): the
//!   promise that the key is free there;
//! - oxrtc answers the picture alone (no sound, so no sound transcode starts
//!   for a reader that drops it);
//! - and lists the reader as a `live` one in `/api/streams` while it reads,
//!   which no test could see before the listing carried the class.
//!
//! Skipped (and passed) when FFmpeg, ffprobe or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use common::launch::wait_healthy;
use common::*;
use tokio::process::Command;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";

/// What `read` found.
struct Read {
    ok: bool,
    frames: u64,
    errors: String,
}

/// FFmpeg as the decoders run it: TCP, the sound dropped, three seconds.
async fn read(tools: &Tools, url: &str) -> Read {
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-rtsp_transport", "tcp", "-i", url])
        .args(["-t", "3", "-an", "-f", "null", "-progress", "pipe:1", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(40), output).await.expect("ffmpeg ran 40 s").unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    let frames = out.lines().filter_map(|l| l.strip_prefix("frame=")).filter_map(|v| v.trim().parse().ok()).next_back();
    Read {
        ok: output.status.success(),
        frames: frames.unwrap_or(0),
        errors: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The classes of the consumers of `stream` that read over RTSP, polled while
/// `reading` runs (a reader is listed only while connected).
async fn classes_while(api: SocketAddr, stream: &str, reading: tokio::task::JoinHandle<Read>) -> (Vec<String>, Read) {
    let mut seen = Vec::new();
    let target = format!("/api/streams?src={stream}");
    while !reading.is_finished() {
        let info = Request::new("GET", &target).auth(USER, PASS).send(api).await.json();
        let consumers = info["consumers"].as_array().cloned().unwrap_or_default();
        let live: Vec<String> = consumers
            .iter()
            .filter(|c| c["format_name"] == "rtsp")
            .filter_map(|c| c["queue"].as_str().map(str::to_owned))
            .collect();
        if !live.is_empty() {
            seen = live;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (seen, reading.await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_analysis_urls_are_read_live_by_oxrtc_and_ignored_by_go2rtc() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start_with_sound(&tools).await;

    // The reference: go2rtc answers the analysis URL as it answers `?video`.
    let reference = camera.url("h264?video&queue=live");
    assert_eq!(codecs(&tools, &reference).await, "h264", "go2rtc ignores queue=live");
    let through_go2rtc = read(&tools, &reference).await;
    assert!(through_go2rtc.ok && through_go2rtc.frames >= 30, "go2rtc: {}", through_go2rtc.errors);

    // openIVR's layout for one camera that has a substream and sound.
    let dir = temp_dir("analysis");
    let (api_port, rtsp_port) = (free_tcp_port(), free_tcp_port());
    let config = format!(
        "api:\n  listen: \"127.0.0.1:{api_port}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp_port}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{}\"\n\
         log:\n  level: info\n\
         streams:\n  camera_1: [\"{}#timeout=10\"]\n  camera_1_sub: [\"{}#timeout=10\"]\n",
        free_webrtc_port(),
        tools.ffmpeg.display(),
        camera.url("with_sound"),
        camera.url("h264"),
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, config).unwrap();
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let mut oxrtc = Oxrtc::start(&path);
    wait_healthy(api).await;

    let base = format!("rtsp://127.0.0.1:{rtsp_port}");
    // The substream (analysis_url()) and the main relay (analysis_url() with
    // no substream, capture_url()): the picture alone, as live readers.
    for stream in ["camera_1_sub", "camera_1"] {
        let url = format!("{base}/{stream}?video&queue=live");
        assert_eq!(codecs(&tools, &url).await, "h264", "{stream}: the picture alone (ffprobe, as the sidecar probes)");
        let reading = tokio::spawn({
            let (tools, url) = (tools.clone(), url.clone());
            async move { read(&tools, &url).await }
        });
        let (classes, result) = classes_while(api, stream, reading).await;
        assert!(result.ok && result.frames >= 30, "{stream}: {} frames, {}", result.frames, result.errors);
        assert!(result.errors.trim().is_empty(), "{stream}: ffmpeg reported {}", result.errors);
        assert_eq!(classes, ["live"], "{stream}: listed as a live reader while it reads");
    }

    // A reader that says nothing is still a recorder: what the segmenter is.
    let url = format!("{base}/camera_1_sub?video");
    let reading = tokio::spawn({
        let tools = tools.clone();
        async move { read(&tools, &url).await }
    });
    let (classes, result) = classes_while(api, "camera_1_sub", reading).await;
    assert!(result.ok, "{}", result.errors);
    assert_eq!(classes, ["recorder"], "no queue= means the deep queue");

    oxrtc.child.kill().await.unwrap();
    let _ = oxrtc.drain().await;
    camera.stop().await;
    remove_dir(&dir).await;
}
