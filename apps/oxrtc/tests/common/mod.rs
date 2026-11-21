//! The end-to-end harness: the binaries' paths, free ports, a minimal HTTP
//! client (`http`), go2rtc as a fake camera, and the oxrtc binary with its log.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

pub mod http;
pub mod launch;
pub mod webcam;
pub mod whep;

// Not every test file sends a request.
#[allow(unused_imports)]
pub use http::{Reply, Request};

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const DEFAULT_FFMPEG: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
const DEFAULT_FFPROBE: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffprobe.exe";
const DEFAULT_GO2RTC: &str = "K:/LABS/CascadeProjects/bench-bin/go2rtc-1.9.15/go2rtc.exe";

/// Says why a test skips itself (a program it needs is not on this machine),
/// and fails it instead where `OXRTC_NO_SKIP` is set, as CI sets it: a skipped
/// test passes, and `cargo test` shows nothing a passing test printed, so a
/// wrong path would turn a job green without running anything.
pub fn skip(why: &str) {
    assert!(std::env::var_os("OXRTC_NO_SKIP").is_none(), "{why}: a skip is a failure where OXRTC_NO_SKIP is set");
    eprintln!("SKIP: {why}");
}

/// The external programs, or `None` (and a SKIP line) when one is missing.
#[derive(Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub go2rtc: PathBuf,
}

impl Tools {
    pub fn find() -> Option<Self> {
        let path = |var: &str, default: &str| PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.into()));
        let tools = Self {
            ffmpeg: path("OXRTC_FFMPEG", DEFAULT_FFMPEG),
            ffprobe: path("OXRTC_FFPROBE", DEFAULT_FFPROBE),
            go2rtc: path("OXRTC_GO2RTC", DEFAULT_GO2RTC),
        };
        for bin in [&tools.ffmpeg, &tools.ffprobe, &tools.go2rtc] {
            if !bin.is_file() {
                skip(&format!("{} not found (set OXRTC_FFMPEG / OXRTC_FFPROBE / OXRTC_GO2RTC)", bin.display()));
                return None;
            }
        }
        Some(tools)
    }
}

/// Where the tests draw the ports they write into a configuration: below the
/// ports a system gives a socket bound to port 0 or a connection's own end
/// (32768 and up on Linux, 49152 and up on Windows). A port is free when it is
/// drawn and bound a moment later by the program it was drawn for; taken from
/// a bind to port 0 and released, it could be handed out again in between, to
/// another test's listener or to any connection (`rtsp.listen` already in use,
/// os error 10048, in the Windows job; "go2rtc did not listen" on a
/// developer's machine). Clear of the ports openIVR's own instances listen on
/// there (21984, 28554).
const DRAWN_PORTS: std::ops::Range<u16> = 22000..28000;

/// The ports this process drew: two tests of one binary never get the same.
static DRAWN: Mutex<std::collections::BTreeSet<u16>> = Mutex::new(std::collections::BTreeSet::new());

/// A port of [`DRAWN_PORTS`] that `free` could bind a moment ago and no other
/// test of this process was given.
fn draw_port(free: impl Fn(u16) -> bool) -> u16 {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..500 {
        // The standard library's per-instance hash seed: random enough here.
        let draw = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let port = DRAWN_PORTS.start + u16::try_from(draw % u64::from(DRAWN_PORTS.end - DRAWN_PORTS.start)).unwrap();
        if free(port) && DRAWN.lock().unwrap().insert(port) {
            return port;
        }
    }
    panic!("no free port in 500 draws of {DRAWN_PORTS:?}");
}

fn tcp_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn udp_is_free(port: u16) -> bool {
    std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
}

/// A port free a moment ago for a TCP listener ([`DRAWN_PORTS`]).
pub fn free_tcp_port() -> u16 {
    draw_port(tcp_is_free)
}

pub fn free_udp_port() -> u16 {
    draw_port(udp_is_free)
}

/// A port free a moment ago for TCP and for UDP: what oxrtc's `webrtc.listen`
/// binds (ICE over both), and oxrtc does not start without every listener.
/// The two are separate questions on Windows, where Hyper-V reserves ranges
/// of a hundred ports per protocol: a port free for UDP can be refused for
/// TCP (os error 10013).
pub fn free_webrtc_port() -> u16 {
    draw_port(|port| tcp_is_free(port) && udp_is_free(port))
}

/// A fresh directory under the system's temporary directory, given to no
/// other caller: named after the process and a count of this process's draws,
/// and created with `create_dir`, which refuses a name that exists (a
/// directory a former process of the same id left is skipped). Named after the
/// clock, two tests drawing in the same tick were given one directory, and
/// each overwrote the other's configuration (the Windows job of 52e3474).
pub fn temp_dir(tag: &str) -> PathBuf {
    static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let draw = DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oxrtc-app-{tag}-{}-{draw}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("cannot create {}: {e}", dir.display()),
        }
    }
}

/// Removes `dir`, retrying while a dying child still holds it.
pub async fn remove_dir(dir: &Path) {
    for _ in 0..50 {
        if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!("could not remove {}", dir.display());
}

// ---------------------------------------------------------------- go2rtc

/// go2rtc serving FFmpeg's H.264 test pattern as `/h264` (and as H.265,
/// `/h265`), behind RTSP authentication, on free ports: the fake camera. With
/// [`FakeCamera::start_with_sound`], `/with_sound` also carries what the
/// owner's cameras send: H.264 and G.711 A-law at 16 kHz, published by FFmpeg.
pub struct FakeCamera {
    child: Child,
    publisher: Option<Child>,
    dir: PathBuf,
    pub rtsp_port: u16,
}

pub const CAMERA_USER: &str = "user";
pub const CAMERA_PASS: &str = "hunter2";

impl FakeCamera {
    pub async fn start(tools: &Tools) -> Self {
        let [api, rtsp, webrtc, srtp] = [free_tcp_port(), free_tcp_port(), free_tcp_port(), free_udp_port()];
        let dir = temp_dir("camera");
        let config = format!(
            "api:\n  listen: \"127.0.0.1:{api}\"\n\
             rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n  username: \"{CAMERA_USER}\"\n  password: \"{CAMERA_PASS}\"\n\
             webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n\
             srtp:\n  listen: \"127.0.0.1:{srtp}\"\n\
             ffmpeg:\n  bin: \"{}\"\n\
             streams:\n  h264: \"ffmpeg:virtual?video=testsrc&size=720#video=h264\"\n\
             \x20 h265: \"ffmpeg:virtual?video=testsrc&size=720#video=h265\"\n  with_sound:\n",
            tools.ffmpeg.display()
        );
        let path = dir.join("go2rtc.yaml");
        std::fs::write(&path, config).unwrap();
        let log_path = dir.join("go2rtc.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let child = Command::new(&tools.go2rtc)
            .arg("-config")
            .arg(&path)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("{} exists but did not start: {e}", tools.go2rtc.display()));
        let mut me = Self { child, publisher: None, dir, rtsp_port: rtsp };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", rtsp)).await.is_err() {
            // Its own words in the failure: a port taken in between, a
            // refused bind and a slow start read the same from outside.
            assert!(
                tokio::time::Instant::now() < deadline,
                "go2rtc did not listen on {rtsp} within 10 s (exited: {:?}); its log:\n{}",
                me.child.try_wait(),
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        me
    }

    /// The camera, and FFmpeg publishing `/with_sound` to it until stopped.
    pub async fn start_with_sound(tools: &Tools) -> Self {
        let mut me = Self::start(tools).await;
        let target = me.url("with_sound");
        let spawn = || {
            Command::new(&tools.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-re"])
                .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000"])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25", "-bf", "0"])
                .args(["-pix_fmt", "yuv420p", "-c:a", "pcm_alaw", "-ar", "16000", "-ac", "1"])
                .args(["-f", "rtsp", "-rtsp_transport", "tcp", &target])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap_or_else(|e| panic!("{} exists but did not start: {e}", tools.ffmpeg.display()))
        };
        me.publisher = Some(spawn());
        // Each probe is bounded: before the publication, ffprobe can wait on
        // go2rtc for longer than the whole deadline. A publisher that exited
        // (go2rtc listening, not yet taking publications) is started again.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let published =
            || async { tokio::time::timeout(Duration::from_secs(5), codecs(tools, &target)).await.unwrap_or_default() };
        let mut restarts = 0;
        loop {
            let seen = published().await;
            // In either order: go2rtc lists a publication's tracks as they
            // came, sound first on some runs, and the camera's order is not
            // what these tests measure (each stream of oxrtc sets its own).
            let mut tracks: Vec<&str> = seen.split(',').collect();
            tracks.sort_unstable();
            if tracks == ["h264", "pcm_alaw"] {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "/with_sound was not published within 30 s (ffprobe saw {seen:?}, {restarts} publisher restarts)"
            );
            if me.publisher.as_mut().is_some_and(|p| p.try_wait().ok().flatten().is_some()) {
                restarts += 1;
                me.publisher = Some(spawn());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        me
    }

    pub fn url(&self, path: &str) -> String {
        format!("rtsp://{CAMERA_USER}:{CAMERA_PASS}@127.0.0.1:{}/{path}", self.rtsp_port)
    }

    /// Kills go2rtc through its handle and removes its directory.
    pub async fn stop(mut self) {
        if let Some(mut publisher) = self.publisher.take() {
            let _ = publisher.kill().await;
        }
        let _ = self.child.kill().await;
        remove_dir(&self.dir).await;
    }
}

// ---------------------------------------------------------------- oxrtc

/// The oxrtc binary, its stdout collected line by line.
pub struct Oxrtc {
    pub child: Child,
    pub lines: Arc<Mutex<Vec<String>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Oxrtc {
    pub fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxrtc"))
            .arg("-config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = stdout.next_line().await {
                eprintln!("oxrtc| {line}");
                sink.lock().unwrap().push(line);
            }
        });
        Self { child, lines, reader }
    }

    pub fn log(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// Waits for the end of its stdout (the process closed it by exiting).
    pub async fn drain(self) -> Vec<String> {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.reader).await;
        self.lines.lock().unwrap().clone()
    }
}

/// Whether `line` has go2rtc's shape: `HH:MM:SS.mmm LVL [module] message`.
pub fn go2rtc_shaped(line: &str) -> bool {
    let mut parts = line.splitn(4, ' ');
    let (Some(time), Some(level), Some(module)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let time_ok = time.len() == 12
        && time.chars().enumerate().all(|(i, c)| match i {
            2 | 5 => c == ':',
            8 => c == '.',
            _ => c.is_ascii_digit(),
        });
    let module_ok = module.len() > 2
        && module.starts_with('[')
        && module.ends_with(']')
        && module[1..module.len() - 1].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    time_ok && ["TRC", "DBG", "INF", "WRN", "ERR"].contains(&level) && module_ok
}

/// `codec_name,width,height` of the first video stream of `url`, read by
/// ffprobe over RTSP/TCP.
pub async fn probe(tools: &Tools, url: &str) -> String {
    let output = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-rtsp_transport", "tcp", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height", "-of", "csv=p=0", "-i", url])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(40), output)
        .await
        .expect("ffprobe took more than 40 s")
        .unwrap_or_else(|e| panic!("{} exists but did not run: {e}", tools.ffprobe.display()));
    assert!(output.status.success(), "ffprobe failed on {url}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The codec names of every stream of `url`, comma-separated (empty when
/// ffprobe cannot read it), read over RTSP/TCP.
pub async fn codecs(tools: &Tools, url: &str) -> String {
    let output = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-rtsp_transport", "tcp", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .args(["-i", url])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(40), output).await {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).lines().map(str::trim).collect::<Vec<_>>().join(",")
        }
        _ => String::new(),
    }
}

fn is_ffmpeg(process: &sysinfo::Process) -> bool {
    process.name().to_ascii_lowercase().starts_with("ffmpeg")
}

/// The FFmpeg processes `parent` started itself (its direct children).
pub fn ffmpeg_children(parent: u32) -> Vec<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    let parent = sysinfo::Pid::from_u32(parent);
    system
        .processes()
        .iter()
        .filter(|(_, process)| process.parent() == Some(parent) && is_ffmpeg(process))
        .map(|(pid, _)| pid.as_u32())
        .collect()
}

/// Those of `pids` that are still a running FFmpeg process. A zombie is dead:
/// killed, it holds no camera and no processor, and only waits to be reaped by
/// whoever adopted it - at once under an init, never under a container's
/// first process that reaps nothing.
pub fn still_running(pids: &[u32]) -> Vec<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    let runs = |process: &sysinfo::Process| process.status() != sysinfo::ProcessStatus::Zombie && is_ffmpeg(process);
    pids.iter().copied().filter(|pid| system.process(sysinfo::Pid::from_u32(*pid)).is_some_and(runs)).collect()
}

/// Width and height from a JPEG's start-of-frame segment.
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u16, u16)> {
    let mut i = 2;
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if matches!(marker, 0xC0..=0xC3) {
            let height = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
            let width = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
            return Some((width, height));
        }
        i += 2 + len;
    }
    None
}
