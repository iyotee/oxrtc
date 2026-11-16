//! The switch-over proof on the owner's real cameras: the binary on a COPY of
//! openIVR's generated `config/go2rtc.yaml`, on free loopback ports, one camera
//! at a time (the cameras ration their connections: oxrtc holds one session
//! to each, a viewer and a reader read through it).
//!
//! Not part of the suite: it talks to cameras on the LAN. Run it with
//!
//! ```text
//! OXRTC_REAL_CONFIG=<path of go2rtc.yaml> OXRTC_REAL_CAMERAS=camera_12,camera_1 \
//!   cargo test -p oxrtc --release --test real_cameras -- --ignored --nocapture
//! ```
//!
//! `OXRTC_REAL_BIN` runs another oxrtc binary (the release one) instead of the
//! one cargo built for the test. The copy changes the listen ports and the
//! loopback relay's port inside the sources, and the WebRTC candidates (the
//! owner's LAN address is not reachable from a loopback client); every stream
//! and every camera source is kept. The API credential is read from the copy
//! and never printed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::whep::play_as;
use common::*;
use str0m::format::Codec;
use tokio::process::Command;

/// The owner's cameras with a microphone: their sound must come out as Opus.
const WITH_SOUND: [&str; 2] = ["camera_1", "camera_2"];
/// What an RTSP read of a stream found.
#[derive(Debug, Default)]
struct Read {
    ok: bool,
    video_kib: f64,
    audio_kib: f64,
    codecs: String,
}

/// The copy of `text` that runs here: ports and candidates only.
fn rewrite(text: &str, api: u16, rtsp: u16, webrtc: u16) -> String {
    let old_rtsp = section_listen(text, "rtsp").and_then(|l| l.rsplit(':').next().map(str::to_owned));
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.starts_with([' ', '\t', '-']) && line.ends_with(':') {
            section = line.trim_end_matches(':').to_owned();
        }
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let line = match (section.as_str(), trimmed) {
            ("api", t) if t.starts_with("listen:") => format!("{indent}listen: \"127.0.0.1:{api}\""),
            ("rtsp", t) if t.starts_with("listen:") => format!("{indent}listen: \"127.0.0.1:{rtsp}\""),
            ("webrtc", t) if t.starts_with("listen:") => format!("{indent}listen: \"127.0.0.1:{webrtc}\""),
            ("webrtc", t) if t.starts_with("candidates:") => format!("{indent}candidates: []"),
            _ => match &old_rtsp {
                Some(old) => line.replace(&format!("127.0.0.1:{old}/"), &format!("127.0.0.1:{rtsp}/")),
                None => line.to_owned(),
            },
        };
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// The value of `listen:` in a top-level `section`.
fn section_listen(text: &str, section: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        if !line.starts_with([' ', '\t', '-']) && line.ends_with(':') {
            inside = line.trim_end_matches(':') == section;
        } else if inside && let Some(value) = line.trim().strip_prefix("listen:") {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// An `api:` key's value.
fn api_value(text: &str, key: &str) -> String {
    let mut inside = false;
    for line in text.lines() {
        if !line.starts_with([' ', '\t', '-']) && line.ends_with(':') {
            inside = line == "api:";
        } else if inside && let Some(value) = line.trim().strip_prefix(&format!("{key}:")) {
            return value.trim().trim_matches(['"', '\'']).to_owned();
        }
    }
    String::new()
}

/// FFmpeg reading every track of `url` for `seconds`, copied to nowhere.
async fn read(tools: &Tools, url: &str, seconds: u32) -> Read {
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "info", "-rtsp_transport", "tcp", "-i", url])
        .args(["-t", &seconds.to_string(), "-map", "0", "-c", "copy", "-f", "null", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(60), output).await else {
        return Read::default();
    };
    let log = String::from_utf8_lossy(&output.stderr);
    let size = |label: &str| {
        log.lines()
            .rev()
            .find(|l| l.contains("video:") && l.contains("audio:"))
            .and_then(|l| l.split(label).nth(1))
            .and_then(|rest| rest.split(|c: char| !(c.is_ascii_digit() || c == '.')).next().map(str::to_owned))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let codecs: Vec<&str> = log
        .lines()
        .take_while(|l| !l.starts_with("Output #0"))
        .filter(|l| l.trim_start().starts_with("Stream #0"))
        .filter_map(|l| l.split(": ").nth(2))
        .filter_map(|l| l.split([' ', ',']).next())
        .collect();
    Read { ok: output.status.success(), video_kib: size("video:"), audio_kib: size("audio:"), codecs: codecs.join("+") }
}

/// The stream's producers and consumers as `GET /api/streams?src=` lists them.
async fn listing(api: SocketAddr, auth: (&str, &str), stream: &str) -> (usize, usize) {
    let reply = Request::new("GET", &format!("/api/streams?src={stream}")).auth(auth.0, auth.1).send(api).await;
    let json = reply.json();
    let count = |key: &str| json.get(key).and_then(|v| v.as_array()).map_or(0, Vec::len);
    (count("producers"), count("consumers"))
}

/// Waits until `stream` has no consumer left, so the next camera starts alone.
async fn idle(api: SocketAddr, auth: (&str, &str), stream: &str) {
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        if listing(api, auth, stream).await.1 == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "talks to the owner's real cameras: set OXRTC_REAL_CONFIG"]
async fn the_owners_six_cameras_through_the_binary() {
    let Ok(config) = std::env::var("OXRTC_REAL_CONFIG") else {
        eprintln!("SKIP: OXRTC_REAL_CONFIG is not set");
        return;
    };
    let Some(tools) = Tools::find() else { return };
    let original = std::fs::read_to_string(&config).unwrap();
    let ports = launch::Ports::free();
    let mut text = rewrite(&original, ports.api, ports.rtsp, ports.webrtc);
    // The reference (`OXRTC_REAL_BIN` naming go2rtc) also listens for SRTP on a port of its own.
    if std::env::var("OXRTC_REAL_BIN").is_ok_and(|bin| bin.to_ascii_lowercase().contains("go2rtc")) {
        text.push_str(&format!(
            "srtp:
  listen: \"127.0.0.1:{}\"
",
            free_tcp_port()
        ));
        text = text.replace("candidates: []", &format!("candidates: [\"127.0.0.1:{}\"]", ports.webrtc));
    }
    if std::env::var_os("OXRTC_REAL_DEBUG").is_some() {
        text = text.replace("level: info", "level: debug");
    }
    let user = api_value(&text, "username");
    let pass = api_value(&text, "password");
    let auth = (user.as_str(), pass.as_str());
    let dir = temp_dir("real");
    let path = dir.join("go2rtc.yaml");
    std::fs::write(&path, &text).unwrap();

    let binary =
        std::env::var("OXRTC_REAL_BIN").map_or_else(|_| PathBuf::from(env!("CARGO_BIN_EXE_oxrtc")), PathBuf::from);
    let log = std::fs::File::create(dir.join("oxrtc.log")).unwrap();
    let mut child = Command::new(&binary)
        .arg("-config")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let wanted = std::env::var("OXRTC_REAL_CAMERAS")
        .unwrap_or_else(|_| "camera_12,camera_5,camera_11,camera_4,camera_1,camera_2".into());
    let (api, rtsp) = (ports.api(), ports.rtsp);
    let mut failures = Vec::new();
    for camera in wanted.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        let sound = WITH_SOUND.contains(&camera);
        let stream_url = format!("rtsp://127.0.0.1:{rtsp}/{camera}");
        let started = Instant::now();
        // A viewer as openIVR names its sessions (sound asked for only where
        // the camera has some), and an RTSP reader one second after it.
        let (viewer, reader) = tokio::join!(
            play_as(api, auth, camera, Some("openivr-proof"), sound, 30, Duration::from_secs(45)),
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                read(&tools, &stream_url, 4).await
            }
        );
        let (producers, consumers) = listing(api, auth, camera).await;
        let opus = viewer.codecs.contains(&Codec::Opus);
        eprintln!(
            "PROOF {camera}: whep status={} answer={:.1}s connected={} frames={} first_keyframe={} keyframes={} first_keyframe_at={:?} codecs={:?} opus={opus};              rtsp ok={} tracks={} video={:.1}KiB audio={:.1}KiB; producers={producers} consumers={consumers}; {:.1}s",
            viewer.status,
            viewer.answer.as_secs_f64(),
            viewer.connected,
            viewer.video_frames,
            viewer.first_keyframe,
            viewer.keyframes,
            viewer.first_keyframe_at,
            viewer.codecs,
            reader.ok,
            reader.codecs,
            reader.video_kib,
            reader.audio_kib,
            started.elapsed().as_secs_f64()
        );
        let mut bad = Vec::new();
        if !viewer.connected || viewer.video_frames < 30 {
            bad.push("whep frames");
        }
        if !reader.ok || reader.video_kib <= 0.0 {
            bad.push("rtsp video");
        }
        if sound && !opus {
            bad.push("whep opus");
        }
        if sound && (reader.audio_kib <= 0.0 || !reader.codecs.contains("opus")) {
            bad.push("rtsp opus");
        }
        if !bad.is_empty() {
            failures.push(format!("{camera}: {}", bad.join(", ")));
        }
        idle(api, auth, camera).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let _ = child.kill().await;
    let log_text = std::fs::read_to_string(dir.join("oxrtc.log")).unwrap_or_default();
    assert!(pass.is_empty() || !log_text.contains(&pass), "the API credential reached the log");
    for line in log_text.lines().take(200) {
        eprintln!("LOG {line}");
    }
    remove_dir(&dir).await;
    assert!(failures.is_empty(), "failed: {failures:?}");
}
