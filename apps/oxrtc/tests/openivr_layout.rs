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
//! The first test leaves openIVR's de-duplicated sound source out
//! (`audio_dedup.py`, an HTTP service of openIVR): the layout without it is
//! the one openIVR writes when that service is not running. The second puts
//! it in, served by a stand-in that answers as it does (a WAV of G.711, or
//! 415 for a camera whose sound is not G.711), and checks that the sound of
//! the whole layout is made with no FFmpeg (W5, D37). Skipped (and passed)
//! when FFmpeg, ffprobe or go2rtc is absent.
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

/// openIVR's `DEDUP_INPUT` template (`_write_config()`).
const DEDUP_INPUT: &str = "-fflags nobuffer -flags low_delay -probesize 32 -i {input}";

/// openIVR's sources for one camera, by stream name; `dedup` is the port of
/// its sound de-duplicator when it runs (`recording_sources()`).
fn camera_streams(name: &str, camera_url: &str, rtsp: u16, dedup: Option<u16>) -> String {
    let src = format!("{name}_src");
    let video = format!("rtsp://127.0.0.1:{rtsp}/{src}?video#timeout={RELAY_STALL}");
    let deduplicated = dedup
        .map(|port| format!("\"ffmpeg:http://127.0.0.1:{port}/{src}#audio=opus#input=dedup_input\", "))
        .unwrap_or_default();
    format!(
        "\x20 {src}: [\"{camera_url}#timeout=10\"]\n\
         \x20 {name}_rec: [\"{video}\", {deduplicated}\"ffmpeg:rtsp://127.0.0.1:{rtsp}/{src}#audio=opus#timeout={RELAY_STALL}\"]\n\
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
    dedup: Option<u16>,
) -> std::path::PathBuf {
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n  dedup_input: \"{DEDUP_INPUT}\"\n\
         log:\n  level: info\n\
         streams:\n{sound}{mute}",
        ffmpeg = tools.ffmpeg.display(),
        sound = camera_streams("camera_1", &camera.url("with_sound"), rtsp, dedup),
        mute = camera_streams("camera_2", &camera.url("h264"), rtsp, dedup),
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
    let config = write_config(&dir, &tools, &camera, api_port, rtsp_port, free_webrtc_port(), None);
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

/// openIVR's sound de-duplicator as oxrtc meets it (`audio_dedup.py`): an
/// HTTP/1.0 server on the loopback that answers `GET /camera_1_src` with an
/// endless WAV of A-law at 16 kHz (a 440 Hz tone, 64 ms a packet, as the
/// owner's cameras send), and every other camera with `415`.
async fn dedup_stand_in() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut request = [0u8; 1024];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..read]).lines().next().unwrap_or_default().to_owned();
                if !line.starts_with("GET /camera_1_src ") {
                    let _ = socket.write_all(b"HTTP/1.0 415 Unsupported Media Type\r\nContent-Length: 0\r\n\r\n").await;
                    return;
                }
                let mut out = b"HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n".to_vec();
                out.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00");
                out.extend_from_slice(&[6, 0, 1, 0, 0x80, 0x3e, 0, 0, 0x80, 0x3e, 0, 0, 1, 0, 8, 0]);
                out.extend_from_slice(b"data\xff\xff\xff\xff");
                if socket.write_all(&out).await.is_err() {
                    return;
                }
                let mut sample = 0u64;
                let mut tick = tokio::time::interval(Duration::from_millis(64));
                loop {
                    tick.tick().await;
                    let packet: Vec<u8> = (0..1024)
                        .map(|_| {
                            sample += 1;
                            let t = sample as f64 / 16_000.0;
                            let v = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8000.0;
                            oxrtc_audio::g711::encode_alaw(v as i16)
                        })
                        .collect();
                    if socket.write_all(&packet).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// FFmpeg reads the recorder's stream as openIVR's recorder does, and decodes
/// its sound: no error line, and the seconds asked for.
async fn decode_the_recorders_sound(tools: &Tools, rtsp: u16) -> (bool, String) {
    let url = format!("rtsp://127.0.0.1:{rtsp}/camera_1_rec");
    let out = tokio::process::Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-v", "error", "-rtsp_transport", "tcp", "-i", &url])
        .args(["-map", "0:a", "-t", "4", "-f", "null", "-"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), out).await.expect("ffmpeg hung").unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The whole layout, the de-duplicator in it, and not one FFmpeg for the
/// sound (W5, D37): the recorder's sound held, decoded by FFmpeg with no
/// error, and heard by a viewer, while oxrtc has no FFmpeg child at all; the
/// camera whose sound the de-duplicator refuses and the relay has not is
/// refused as fast as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openivr_layout_with_its_deduplicator_makes_every_sound_with_no_ffmpeg() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let dedup = dedup_stand_in().await;
    let dir = temp_dir("layout-native");
    let (api_port, rtsp_port) = (free_tcp_port(), free_tcp_port());
    let config = write_config(&dir, &tools, &camera, api_port, rtsp_port, free_webrtc_port(), Some(dedup));
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    let pid = oxrtc.child.id().unwrap();
    wait_healthy(api).await;

    let held = hold(api, "camera_1").await;
    assert_eq!(held.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [200], "{held:?}");
    let held = hold(api, "camera_2").await;
    assert_eq!(held.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [500, 200], "{held:?}");
    assert!(held[0].1 < Duration::from_secs(10), "{held:?}");
    assert_eq!(ffmpeg_children(pid), Vec::<u32>::new(), "an FFmpeg for the held sound");

    let rec = codecs(&tools, &format!("rtsp://127.0.0.1:{rtsp_port}/camera_1_rec")).await;
    assert_eq!(rec, "h264,opus", "camera_1_rec");
    let (decoded, errors) = decode_the_recorders_sound(&tools, rtsp_port).await;
    assert!(decoded && errors.trim().is_empty(), "FFmpeg decoding the recorder's Opus: {errors}");

    let viewer = tokio::spawn(whep::play(api, (USER, PASS), "camera_1", true, 20, Duration::from_secs(25)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(ffmpeg_children(pid), Vec::<u32>::new(), "an FFmpeg while a viewer listens");
    let sound = viewer.await.unwrap();
    assert_eq!(sound.status, 201, "{sound:?}");
    assert!(sound.codecs.contains(&str0m::format::Codec::Opus), "the viewer hears the camera: {sound:?}");

    // Where the de-duplicated source serves, the relay's sound is not read.
    let listing = Request::new("GET", "/api/streams?src=camera_1_rec").auth(USER, PASS).send(api).await.json();
    eprintln!("camera_1_rec: {listing}");
    let producers = listing["producers"].as_array().cloned().unwrap_or_default();
    let serving: Vec<&str> = producers
        .iter()
        .filter(|p| p["medias"].as_array().is_some_and(|m| !m.is_empty()))
        .filter_map(|p| p["url"].as_str())
        .collect();
    assert!(serving.iter().any(|url| url.starts_with("ffmpeg:http://")), "{serving:?}");

    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let log = oxrtc.drain().await;
    camera.stop().await;
    remove_dir(&dir).await;
    for line in &log {
        assert!(!line.contains(CAMERA_PASS) && !line.contains(PASS), "a password in the log: {line}");
    }
}
