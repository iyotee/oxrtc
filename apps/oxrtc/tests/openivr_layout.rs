//! The binary on openIVR's own stream layout, the one its go2rtc_manager.py
//! writes (`recording_sources()` and `derived_sources()`), for two cameras:
//! one that sends sound (H.264 and G.711 A-law at 16 kHz, as the owner's do)
//! and one without a microphone. e2e.rs checks every call on a simpler
//! layout; this file checks what the switch-over would meet:
//!
//! - `camera_N_src`: the camera itself;
//! - `camera_N_rec`: the camera's video relayed over the loopback (`?video`
//!   only), then its sound transcoded to Opus. What the recorder records,
//!   held open by a preload of `audio=opus`, and where viewers' sound comes
//!   from;
//! - `camera_N`: the relayed video, the H.264 transcode, then the sound read
//!   from `camera_N_rec?audio=opus`. What a viewer watches, with an Opus
//!   transceiver in every offer.
//!
//! openIVR's de-duplicated sound source (`audio_dedup.py`, an HTTP service of
//! openIVR) is left out: the layout without it is the one openIVR writes when
//! that service is not running. Skipped (and passed) when FFmpeg, ffprobe or
//! go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::launch::wait_healthy;
use common::*;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
/// openIVR's `RELAY_STALL_SECONDS`.
const RELAY_STALL: u32 = 20;

/// openIVR's sources for one camera, by stream name.
fn camera_streams(name: &str, camera_url: &str, rtsp: u16) -> String {
    let src = format!("{name}_src");
    let video = format!("rtsp://127.0.0.1:{rtsp}/{src}?video#timeout={RELAY_STALL}");
    format!(
        "\x20 {src}: [\"{camera_url}#timeout=10\"]\n\
         \x20 {name}_rec: [\"{video}\", \"ffmpeg:rtsp://127.0.0.1:{rtsp}/{src}#audio=opus#timeout={RELAY_STALL}\"]\n\
         \x20 {name}: [\"{video}\", \"ffmpeg:{src}#video=h264#width=1920#height=1080#timeout={RELAY_STALL}\", \
         \"rtsp://127.0.0.1:{rtsp}/{name}_rec?audio=opus#timeout={RELAY_STALL}\"]\n"
    )
}

fn write_config(
    dir: &std::path::Path,
    tools: &Tools,
    camera: &FakeCamera,
    api: u16,
    rtsp: u16,
    webrtc: u16,
) -> std::path::PathBuf {
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n{sound}{mute}",
        ffmpeg = tools.ffmpeg.display(),
        sound = camera_streams("camera_1", &camera.url("with_sound"), rtsp),
        mute = camera_streams("camera_2", &camera.url("h264"), rtsp),
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

/// openIVR's `hold_open()`: the sound first, then the video when the sound
/// is refused with a 500. The status of each request, and how long it took.
async fn hold(api: SocketAddr, camera: &str) -> Vec<(u16, Duration)> {
    let mut answers = Vec::new();
    for media in ["audio=opus", "video="] {
        let target = format!("/api/preload?src={camera}_rec&{media}");
        let started = Instant::now();
        let reply = Request::new("PUT", &target).auth(USER, PASS).send(api).await;
        answers.push((reply.status, started.elapsed()));
        if reply.status != 500 {
            break;
        }
        eprintln!("{camera}: {media} refused: {}", reply.text());
    }
    answers
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openivr_layout_plays_with_and_without_a_microphone() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let dir = temp_dir("layout");
    let (api_port, rtsp_port) = (free_tcp_port(), free_tcp_port());
    let config = write_config(&dir, &tools, &camera, api_port, rtsp_port, free_webrtc_port());
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;

    // The recorder's hold: the sound is held on the camera that has one ...
    let held = hold(api, "camera_1").await;
    eprintln!("camera_1 hold: {held:?}");
    assert_eq!(held.len(), 1, "the sound is held at the first request: {held:?}");
    assert_eq!(held[0].0, 200);
    // ... and refused at once on the one that has none, so the video hold
    // follows. Before 2026-11-15 the refusal came after the 30 s preload wait.
    let held = hold(api, "camera_2").await;
    eprintln!("camera_2 hold: {held:?}");
    assert_eq!(held.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [500, 200], "{held:?}");
    assert!(held[0].1 < Duration::from_secs(10), "the missing sound is refused within 10 s: {held:?}");

    // What the recorder reads: the camera's own video and the Opus sound.
    let rec = codecs(&tools, &format!("rtsp://127.0.0.1:{rtsp_port}/camera_1_rec")).await;
    assert_eq!(rec, "h264,opus", "camera_1_rec");

    // Viewers, with an Opus transceiver as openIVR's live view offers.
    let sound = whep::play(api, (USER, PASS), "camera_1", true, 20, Duration::from_secs(25)).await;
    eprintln!("camera_1 viewer: {sound:?}");
    assert_eq!(sound.status, 201, "{sound:?}");
    assert!(sound.video_frames >= 20, "{sound:?}");
    assert!(sound.codecs.contains(&str0m::format::Codec::Opus), "the viewer hears the camera: {sound:?}");

    let mute = whep::play(api, (USER, PASS), "camera_2", true, 20, Duration::from_secs(25)).await;
    eprintln!("camera_2 viewer: {mute:?}");
    // Before 2026-11-15: 500 "no tracks within 10000 ms", on every attempt.
    assert_eq!(mute.status, 201, "a camera without a microphone is watched: {mute:?}");
    assert!(mute.answer < Duration::from_secs(8), "{mute:?}");
    assert!(mute.video_frames >= 20, "{mute:?}");
    assert_eq!(mute.codecs, [str0m::format::Codec::H264], "{mute:?}");

    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let log = oxrtc.drain().await;
    camera.stop().await;
    remove_dir(&dir).await;
    for line in &log {
        assert!(!line.contains(CAMERA_PASS) && !line.contains(PASS), "a password in the log: {line}");
    }
}
