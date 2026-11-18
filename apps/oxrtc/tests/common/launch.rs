//! What the tests that launch the binary share: a configuration with a held
//! transcode, the job object of a parent, and a Python to play a console's
//! owner.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{FakeCamera, Request, Tools, free_tcp_port, free_webrtc_port};

pub const API_USER: &str = "admin";
pub const API_PASS: &str = "s3cret-api";

/// The three ports of a run.
pub struct Ports {
    pub api: u16,
    pub rtsp: u16,
    pub webrtc: u16,
}

impl Ports {
    pub fn free() -> Self {
        Self { api: free_tcp_port(), rtsp: free_tcp_port(), webrtc: free_webrtc_port() }
    }

    pub fn api(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.api))
    }

    pub fn rtsp(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.rtsp))
    }
}

/// Waits until the binary just launched answers `GET /api` with 200, for
/// [`API_USER`]: it is up. Until its listener is bound a connection is refused
/// (at once on Linux; Windows retries for two seconds first), which means
/// "not yet" and never fails the wait: only the deadline does.
pub async fn wait_healthy(api: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(api).await.is_ok()
            && Request::new("GET", "/api").auth(API_USER, API_PASS).send(api).await.status == 200
        {
            return;
        }
        assert!(Instant::now() < deadline, "GET /api did not answer 200 within 10 s of launch");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A configuration with the camera as `cam_src` and a transcode of it,
/// `cam_small`, which `PUT /api/preload?src=cam_small&video=` holds open
/// (one FFmpeg under oxrtc).
pub fn transcode_config(dir: &Path, tools: &Tools, camera: &FakeCamera, ports: &Ports) -> PathBuf {
    let Ports { api, rtsp, webrtc } = ports;
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{API_USER}\"\n  password: \"{API_PASS}\"\n\
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
    path
}

/// A kill-on-close job object holding `child`, as openIVR's backend makes one
/// (`app/core/process_guard.py`, `_assign_to_job`). Dropped, it ends what it
/// holds, and what that started after.
#[cfg(windows)]
pub fn kill_on_close_job(child: &tokio::process::Child) -> win32job::Job {
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&info).unwrap();
    job.assign_process(child.raw_handle().unwrap() as isize).unwrap();
    job
}

/// A Python that runs: `OXRTC_PYTHON`, else `python` and `python3` of the
/// `PATH` (the Microsoft Store's stub, which opens the Store instead, does not
/// print what it is asked to).
pub fn find_python() -> Option<PathBuf> {
    let candidates =
        std::env::var_os("OXRTC_PYTHON").map(PathBuf::from).into_iter().chain(["python".into(), "python3".into()]);
    for candidate in candidates {
        let output = std::process::Command::new(&candidate).args(["-c", "print(6 * 7)"]).output();
        if output.is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"42")) {
            return Some(candidate);
        }
    }
    super::skip("no Python found (set OXRTC_PYTHON)");
    None
}
