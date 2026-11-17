//! R13: oxrtc killed without a chance to clean up (SIGKILL on Linux,
//! TerminateProcess on Windows) takes its FFmpeg processes along within a
//! second - on Linux through `oxrtc-exec`'s death signal, on Windows through
//! the job object. On Linux they also ran confined (R29). Skipped (and
//! passed) when FFmpeg, ffprobe or go2rtc is absent, like `e2e.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::{Duration, Instant};

use common::launch::*;
use common::*;

const USER: &str = API_USER;
const PASS: &str = API_PASS;

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
    killed_takes_ffmpeg_along(false).await;
}

/// openIVR puts its media server in a kill-on-close job object of its own
/// right after spawning it (`bind_to_parent`, `app/core/process_guard.py`),
/// and stops it with `TerminateProcess`. oxrtc's job for each FFmpeg is then
/// nested in openIVR's: it must still be made (no `no job object` warning)
/// and still end FFmpeg with oxrtc, not only with openIVR.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inside_its_parents_job_a_killed_oxrtc_still_takes_its_ffmpeg_along() {
    killed_takes_ffmpeg_along(true).await;
}

async fn killed_takes_ffmpeg_along(in_parents_job: bool) {
    let Some(tools) = Tools::find() else { return };
    let linux = cfg!(target_os = "linux");
    let wrapper = std::path::Path::new(env!("CARGO_BIN_EXE_oxrtc")).with_file_name("oxrtc-exec");
    assert!(!linux || wrapper.is_file(), "{} not built: run cargo test --workspace", wrapper.display());
    let camera = FakeCamera::start(&tools).await;
    let dir = temp_dir("killed");
    let api = Ports::free();
    let config = transcode_config(&dir, &tools, &camera, &api);
    let api = api.api();

    let mut oxrtc = Oxrtc::start(&config);
    // Held to the end: dropped, it would end oxrtc itself.
    let _parents_job = in_parents_job.then(|| parents_job(&oxrtc.child));
    wait_healthy(api).await;
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
        let left = still_running(&transcoders);
        if left.is_empty() {
            break;
        }
        assert!(killed.elapsed() < Duration::from_secs(1), "FFmpeg {left:?} outlived a killed oxrtc by 1 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!("{} FFmpeg gone {:?} after the kill", transcoders.len(), killed.elapsed());
    let log = oxrtc.drain().await;
    assert!(!log.iter().any(|l| l.contains("no job object")), "every FFmpeg had its job: {log:#?}");
    camera.stop().await;
    remove_dir(&dir).await;
}

#[cfg(windows)]
fn parents_job(child: &tokio::process::Child) -> win32job::Job {
    kill_on_close_job(child)
}

/// No job objects elsewhere: the parent's death reaches oxrtc by signal.
#[cfg(not(windows))]
fn parents_job(_child: &tokio::process::Child) {}
