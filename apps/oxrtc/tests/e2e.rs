//! The built binary against a real camera: go2rtc serving FFmpeg's H.264
//! test pattern plays the camera, oxrtc runs from a go2rtc-format
//! configuration on free ports, and every call openIVR makes is checked
//! from outside the process. Skipped (and passed) when FFmpeg, ffprobe or
//! go2rtc is absent; failed when one is present but broken.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";

struct Ports {
    api: u16,
    rtsp: u16,
    webrtc: u16,
}

fn write_config(dir: &std::path::Path, tools: &Tools, camera: &FakeCamera, ports: &Ports) -> std::path::PathBuf {
    let Ports { api, rtsp, webrtc } = ports;
    // openIVR's own shapes: flow-style lists, a loopback relay first, then
    // the transcode of the relayed stream.
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n\
         \x20 cam_src: [\"{camera}#timeout=10\"]\n\
         \x20 cam: [\"rtsp://127.0.0.1:{rtsp}/cam_src?video\", \"ffmpeg:cam_src#video=h264#width=640#height=360\"]\n\
         \x20 cam_small: [\"ffmpeg:cam_src#video=h264#width=640#height=360\"]\n",
        ffmpeg = tools.ffmpeg.display(),
        camera = camera.url("h264"),
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

async fn wait_healthy(api: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(api).await.is_ok() {
            let reply = Request::new("GET", "/api").auth(USER, PASS).send(api).await;
            if reply.status == 200 {
                return;
            }
        }
        assert!(Instant::now() < deadline, "GET /api did not answer 200 within 10 s of launch");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn check_api(api: SocketAddr) {
    let anonymous = Request::new("GET", "/api").send(api).await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.text());
    let wrong = Request::new("GET", "/api").auth(USER, "nope").send(api).await;
    assert_eq!(wrong.status, 401);
    let reply = Request::new("GET", "/api").auth(USER, PASS).send(api).await;
    assert_eq!(reply.status, 200);
    assert!(reply.json()["version"].is_string(), "{}", reply.text());
}

async fn check_preload(api: SocketAddr) {
    let started = Instant::now();
    let reply = Request::new("PUT", "/api/preload?src=cam_src&video=").auth(USER, PASS).send(api).await;
    assert_eq!(reply.status, 200, "preload: {}", reply.text());
    eprintln!("preload held after {:?}", started.elapsed());
    let streams = Request::new("GET", "/api/streams").auth(USER, PASS).send(api).await;
    assert_eq!(streams.status, 200);
    assert!(!streams.text().contains(CAMERA_PASS), "a camera password in /api/streams: {}", streams.text());
    let info = streams.json();
    let cam_src = &info["cam_src"];
    let producer = &cam_src["producers"][0];
    let medias = producer["medias"].as_array().expect("medias");
    assert!(medias.iter().any(|m| m.as_str().is_some_and(|m| m.starts_with("video") && m.contains("H264"))), "{info}");
    let consumers = cam_src["consumers"].as_array().expect("consumers");
    assert!(consumers.iter().any(|c| c["format_name"] == "preload"), "{info}");
}

async fn check_rtsp(tools: &Tools, rtsp: u16) {
    // The relay of the camera's own stream, then the transcode.
    let relayed = probe(tools, &format!("rtsp://127.0.0.1:{rtsp}/cam?video")).await;
    assert!(relayed.starts_with("h264,"), "cam?video: {relayed}");
    let small = probe(tools, &format!("rtsp://127.0.0.1:{rtsp}/cam_small?video")).await;
    assert_eq!(small, "h264,640,360", "the ffmpeg: transcode");
}

async fn check_still(api: SocketAddr) {
    let reply = Request::new("GET", "/api/frame.jpeg?src=cam_src&width=320").auth(USER, PASS).send(api).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("content-type"), Some("image/jpeg"));
    assert!(reply.body.starts_with(&[0xFF, 0xD8]) && reply.body.ends_with(&[0xFF, 0xD9]), "not a JPEG");
    assert_eq!(jpeg_size(&reply.body).map(|(w, _)| w), Some(320));
}

async fn check_refusals(api: SocketAddr) {
    let exec = Request::new("PUT", "/api/streams?name=evil&src=exec:calc.exe").auth(USER, PASS).send(api).await;
    assert_eq!(exec.status, 400, "{}", exec.text());
    let streams = Request::new("GET", "/api/streams?src=evil").auth(USER, PASS).send(api).await;
    assert_eq!(streams.status, 404, "the refused stream does not exist");
}

async fn check_whep(api: SocketAddr) {
    let played = whep::play(api, (USER, PASS), "cam", false, 20, Duration::from_secs(20)).await;
    assert_eq!(played.status, 201);
    assert!(played.connected, "{played:?}");
    assert!(played.video_frames >= 20, "{played:?}");
    assert!(played.first_keyframe, "the first frame is a keyframe (GOP cache): {played:?}");
    assert_eq!(played.codecs, [str0m::format::Codec::H264]);
}

/// Holds the `ffmpeg:` transcode open (as openIVR's preloads do) and returns
/// the FFmpeg processes oxrtc runs: the stop must take them along.
async fn hold_transcode(api: SocketAddr, oxrtc: &Oxrtc) -> Vec<u32> {
    let reply = Request::new("PUT", "/api/preload?src=cam_small&video=").auth(USER, PASS).send(api).await;
    assert_eq!(reply.status, 200, "preload of the transcode: {}", reply.text());
    let pid = oxrtc.child.id().expect("running");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let children = ffmpeg_children(pid);
        if !children.is_empty() {
            return children;
        }
        assert!(Instant::now() < deadline, "no FFmpeg under oxrtc while a transcode is held");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stops the process the way the platform allows from a test (on Windows
/// the child's kill, which is what openIVR's `terminate()` does there), and
/// checks that it, its listeners and its FFmpeg processes are gone within 5 s.
async fn stop(mut oxrtc: Oxrtc, api: SocketAddr, rtsp: SocketAddr, transcoders: &[u32]) -> Vec<String> {
    let started = Instant::now();
    #[cfg(unix)]
    {
        let pid = oxrtc.child.id().expect("running").to_string();
        let status = std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(status.success());
        let exit = tokio::time::timeout(Duration::from_secs(5), oxrtc.child.wait())
            .await
            .expect("still running 5 s after SIGTERM")
            .unwrap();
        assert!(exit.success(), "a signal-driven stop exits 0: {exit}");
    }
    #[cfg(not(unix))]
    {
        // No Ctrl+C can be sent to a child without unsafe code: kill it
        // through its handle (TerminateProcess, openIVR's `terminate()`), then
        // check that the listeners and the FFmpeg processes are gone.
        oxrtc.child.kill().await.unwrap();
    }
    // A listener on loopback accepts within milliseconds; a refused connect
    // takes about 2 s on Windows (it retries), so a slow answer means "gone".
    let listening = |addr| async move {
        matches!(
            tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(addr)).await,
            Ok(Ok(_))
        )
    };
    for addr in [api, rtsp] {
        while listening(addr).await {
            assert!(started.elapsed() < Duration::from_secs(5), "{addr} still listening 5 s after the stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    loop {
        let left = still_running(transcoders);
        if left.is_empty() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "FFmpeg {left:?} outlived oxrtc by 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("stopped in {:?}, with {} FFmpeg", started.elapsed(), transcoders.len());
    oxrtc.drain().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_binary_serves_what_openivr_uses() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start(&tools).await;
    let dir = temp_dir("e2e");
    let ports = Ports { api: free_tcp_port(), rtsp: free_tcp_port(), webrtc: free_udp_port() };
    let config = write_config(&dir, &tools, &camera, &ports);
    let api = SocketAddr::from(([127, 0, 0, 1], ports.api));
    let rtsp = SocketAddr::from(([127, 0, 0, 1], ports.rtsp));

    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;
    check_api(api).await;
    check_preload(api).await;
    check_refusals(api).await;
    check_still(api).await;
    check_rtsp(&tools, ports.rtsp).await;
    check_whep(api).await;
    assert!(!oxrtc.log().is_empty());
    let transcoders = hold_transcode(api, &oxrtc).await;

    let log = stop(oxrtc, api, rtsp, &transcoders).await;
    camera.stop().await;
    remove_dir(&dir).await;

    for line in &log {
        assert!(go2rtc_shaped(line), "not in go2rtc's shape: {line:?}");
        assert!(!line.contains(CAMERA_PASS) && !line.contains(PASS), "a password in the log: {line}");
    }
    let has = |needle: &str| log.iter().any(|l| l.contains(needle));
    assert!(has(" INF [api] listen addr=127.0.0.1:"), "{log:#?}");
    assert!(has(" INF [rtsp] listen addr=127.0.0.1:"), "{log:#?}");
    assert!(has(" INF [api] preload held"), "{log:#?}");
    assert!(has(" WRN [api] stream refused"), "{log:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_in_use_exits_1_naming_the_address() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap();
    let dir = temp_dir("busy");
    let text = format!(
        "api:\n  listen: \"127.0.0.1:0\"\n  username: \"u\"\n  password: \"p\"\n\
         rtsp:\n  listen: \"{busy}\"\nwebrtc:\n  listen: \"127.0.0.1:0\"\n"
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    let mut oxrtc = Oxrtc::start(&path);
    let exit = tokio::time::timeout(Duration::from_secs(10), oxrtc.child.wait()).await.expect("did not exit").unwrap();
    assert_eq!(exit.code(), Some(1));
    let log = oxrtc.drain().await;
    remove_dir(&dir).await;
    assert!(log.iter().any(|l| l.contains(" ERR ") && l.contains(&busy.to_string())), "{log:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_credentials_no_start() {
    let dir = temp_dir("open");
    let text =
        "api:\n  listen: \"127.0.0.1:0\"\nrtsp:\n  listen: \"127.0.0.1:0\"\nwebrtc:\n  listen: \"127.0.0.1:0\"\n";
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    let mut oxrtc = Oxrtc::start(&path);
    let exit = tokio::time::timeout(Duration::from_secs(10), oxrtc.child.wait()).await.expect("did not exit").unwrap();
    assert_eq!(exit.code(), Some(1));
    let log = oxrtc.drain().await;
    remove_dir(&dir).await;
    assert!(log.iter().any(|l| l.contains(" ERR ") && l.contains("api.allow_unauthenticated")), "{log:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_api_is_a_logged_opt_in() {
    let dir = temp_dir("opened");
    let text = "api:\n  listen: \"127.0.0.1:0\"\n  allow_unauthenticated: true\n\
                rtsp:\n  listen: \"127.0.0.1:0\"\nwebrtc:\n  listen: \"127.0.0.1:0\"\n";
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    let mut oxrtc = Oxrtc::start(&path);
    let deadline = Instant::now() + Duration::from_secs(10);
    let warned = |log: &[String]| log.iter().any(|l| l.contains(" WRN [api] ") && l.contains("allow_unauthenticated"));
    while !warned(&oxrtc.log()) {
        assert!(Instant::now() < deadline, "no warning about the open API: {:#?}", oxrtc.log());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    oxrtc.child.kill().await.unwrap();
    let _ = oxrtc.drain().await;
    remove_dir(&dir).await;
}
