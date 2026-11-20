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
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    // Who had the port a second in, if the binary was not up by then: kept
    // for the failure's message, since whoever it was may be gone at the end.
    let mut early = None;
    loop {
        if tokio::net::TcpStream::connect(api).await.is_ok()
            && Request::new("GET", "/api").auth(API_USER, API_PASS).send(api).await.status == 200
        {
            return;
        }
        if early.is_none() && started.elapsed() > Duration::from_secs(1) {
            early = Some(holders(api.port()));
        }
        assert!(
            Instant::now() < deadline,
            "GET /api did not answer 200 within 10 s of launch; port {} was held after 1 s by [{}] and at the end by [{}]",
            api.port(),
            early.unwrap_or_default(),
            holders(api.port())
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What listens on `port` now, as the system tells it (`netstat`, and
/// `tasklist` for the name of each owner; `ss` elsewhere), for a failed
/// launch's message. A port is free when a test draws it
/// (`super::free_tcp_port`), and a launch that found it taken a second later
/// (`cannot listen on 127.0.0.1:27662 (api.listen) ... os error 10048`, the
/// Windows job of CI run 44629451943) said nothing of who had taken it.
pub fn holders(port: u16) -> String {
    let (program, args): (&str, &[&str]) = if cfg!(windows) { ("netstat", &["-ano"]) } else { ("ss", &["-Hlntup"]) };
    let Ok(listed) = std::process::Command::new(program).args(args).output() else {
        return format!("{program} did not run");
    };
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    let mut found = Vec::new();
    for line in text.lines().filter(|line| line.split_whitespace().any(|word| word.ends_with(&format!(":{port}")))) {
        let mut said = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let owner = line.split_whitespace().last().filter(|pid| cfg!(windows) && pid.parse::<u32>().is_ok());
        if let Some(pid) = owner {
            let filter = format!("PID eq {pid}");
            let named = std::process::Command::new("tasklist").args(["/FI", &filter, "/FO", "CSV", "/NH"]).output();
            if let Ok(named) = named {
                said = format!("{said} = {}", String::from_utf8_lossy(&named.stdout).trim());
            }
        }
        found.push(said);
    }
    found.join("; ")
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
