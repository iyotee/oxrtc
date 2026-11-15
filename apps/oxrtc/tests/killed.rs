//! R13: oxrtc killed without a chance to clean up (SIGKILL on Linux,
//! TerminateProcess on Windows) takes its FFmpeg processes along within a
//! second - on Linux through `oxrtc-exec`'s death signal, on Windows through
//! the job object. On Linux they also ran confined (R29). Skipped (and
//! passed) when FFmpeg, ffprobe or go2rtc is absent, like `e2e.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";

fn write_config(dir: &std::path::Path, tools: &Tools, camera: &FakeCamera) -> (std::path::PathBuf, SocketAddr) {
    let (api, rtsp, webrtc) = (free_tcp_port(), free_tcp_port(), free_udp_port());
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n\
         \x20 cam_src: [\"{camera}#timeout=10\"]\n\
         \x20 cam_small: [\"ffmpeg:cam_src#video=h264#width=320#height=180\"]\n",
        ffmpeg = tools.ffmpeg.display(),
        camera = camera.url("h264"),
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    (path, SocketAddr::from(([127, 0, 0, 1], api)))
}

/// Those of `pids` still running as FFmpeg: a zombie (killed, not yet
/// reaped by whoever adopted it) is dead.
fn alive(pids: &[u32]) -> Vec<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    pids.iter()
        .copied()
        .filter(|pid| {
            system.process(sysinfo::Pid::from_u32(*pid)).is_some_and(|p| {
                p.status() != sysinfo::ProcessStatus::Zombie && p.name().to_ascii_lowercase().starts_with("ffmpeg")
            })
        })
        .collect()
}

/// What `/proc` says of a confined FFmpeg (Linux).
fn check_confined(pid: u32) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(status.lines().any(|l| l.split_whitespace().eq(["NoNewPrivs:", "1"])), "{status}");
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let row = |name: &str| {
        let line = limits.lines().find(|l| l.starts_with(name)).unwrap();
        line[name.len()..].split_whitespace().take(2).collect::<Vec<_>>()
    };
    assert_eq!(row("Max open files"), ["256", "256"], "{limits}");
    assert_eq!(row("Max file size"), ["0", "0"], "{limits}");
    assert_eq!(row("Max core file size"), ["0", "0"], "{limits}");
    assert_eq!(row("Max address space"), ["4294967296", "4294967296"], "{limits}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_oxrtc_takes_its_ffmpeg_along_within_a_second() {
    let Some(tools) = Tools::find() else { return };
    let linux = cfg!(target_os = "linux");
    let wrapper = std::path::Path::new(env!("CARGO_BIN_EXE_oxrtc")).with_file_name("oxrtc-exec");
    assert!(!linux || wrapper.is_file(), "{} not built: run cargo test --workspace", wrapper.display());
    let camera = FakeCamera::start(&tools).await;
    let dir = temp_dir("killed");
    let (config, api) = write_config(&dir, &tools, &camera);

    let mut oxrtc = Oxrtc::start(&config);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Request::new("GET", "/api").auth(USER, PASS).send(api).await.status != 200 {
        assert!(Instant::now() < deadline, "GET /api did not answer within 10 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reply = Request::new("PUT", "/api/preload?src=cam_small&video=").auth(USER, PASS).send(api).await;
    assert_eq!(reply.status, 200, "preload of the transcode: {}", reply.text());
    let pid = oxrtc.child.id().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let transcoders = loop {
        let children = ffmpeg_children(pid);
        if !children.is_empty() {
            break children;
        }
        assert!(Instant::now() < deadline, "no FFmpeg under oxrtc while a transcode is held");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if linux {
        transcoders.iter().for_each(|&pid| check_confined(pid));
        assert!(oxrtc.log().iter().any(|l| l.contains("confined (dies with oxrtc")), "{:#?}", oxrtc.log());
    }

    oxrtc.child.kill().await.unwrap(); // SIGKILL, or TerminateProcess: no clean-up.
    let killed = Instant::now();
    loop {
        let left = alive(&transcoders);
        if left.is_empty() {
            break;
        }
        assert!(killed.elapsed() < Duration::from_secs(1), "FFmpeg {left:?} outlived a killed oxrtc by 1 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!("{} FFmpeg gone {:?} after the kill", transcoders.len(), killed.elapsed());
    camera.stop().await;
    remove_dir(&dir).await;
}
