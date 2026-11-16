//! The end-to-end harness: the binaries' paths, free ports, a minimal HTTP
//! client, go2rtc as a fake camera, and the oxrtc binary with its log.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

pub mod whep;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

const DEFAULT_FFMPEG: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
const DEFAULT_FFPROBE: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffprobe.exe";
const DEFAULT_GO2RTC: &str = "K:/LABS/CascadeProjects/bench-bin/go2rtc-1.9.15/go2rtc.exe";

/// The external programs, or `None` (and a SKIP line) when one is missing.
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
                eprintln!("SKIP: {} not found (set OXRTC_FFMPEG / OXRTC_FFPROBE / OXRTC_GO2RTC)", bin.display());
                return None;
            }
        }
        Some(tools)
    }
}

/// A port free a moment ago (bound on 127.0.0.1:0, then released).
pub fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A fresh directory under the system's temporary directory.
pub fn temp_dir(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("oxrtc-app-{tag}-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

// ---------------------------------------------------------------- HTTP

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub auth: Option<(&'a str, &'a str)>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    /// More request headers, as (name, value).
    pub headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Request<'a> {
    pub fn new(method: &'a str, target: &'a str) -> Self {
        Self { method, target, auth: None, content_type: None, body: &[], headers: Vec::new() }
    }

    pub fn header(mut self, name: &'a str, value: &'a str) -> Self {
        self.headers.push((name, value));
        self
    }

    pub fn auth(mut self, user: &'a str, pass: &'a str) -> Self {
        self.auth = Some((user, pass));
        self
    }

    pub fn body(mut self, content_type: &'a str, body: &'a [u8]) -> Self {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }

    /// One request on its own connection (`Connection: close`).
    pub async fn send(&self, addr: std::net::SocketAddr) -> Reply {
        tokio::time::timeout(Duration::from_secs(70), self.exchange(addr))
            .await
            .unwrap_or_else(|_| panic!("{} {} timed out", self.method, self.target))
    }

    async fn exchange(&self, addr: std::net::SocketAddr) -> Reply {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let mut head = format!("{} {} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n", self.method, self.target);
        if let Some((user, pass)) = self.auth {
            let credential = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            head.push_str(&format!("Authorization: Basic {credential}\r\n"));
        }
        if let Some(content_type) = self.content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(self.body).await.unwrap();
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.unwrap();
        parse_reply(&raw)
    }
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("no end of headers");
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    Reply { status, headers, body }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let end = data.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let size = usize::from_str_radix(std::str::from_utf8(&data[..end]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[end + 2..end + 2 + size]);
        data = &data[end + 4 + size..];
    }
}

// ---------------------------------------------------------------- go2rtc

/// go2rtc serving FFmpeg's H.264 test pattern as `/h264`, behind RTSP
/// authentication, on free ports: the fake camera. With
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
             streams:\n  h264: \"ffmpeg:virtual?video=testsrc&size=720#video=h264\"\n  with_sound:\n",
            tools.ffmpeg.display()
        );
        let path = dir.join("go2rtc.yaml");
        std::fs::write(&path, config).unwrap();
        let child = Command::new(&tools.go2rtc)
            .arg("-config")
            .arg(&path)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("{} exists but did not start: {e}", tools.go2rtc.display()));
        let me = Self { child, publisher: None, dir, rtsp_port: rtsp };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", rtsp)).await.is_err() {
            assert!(tokio::time::Instant::now() < deadline, "go2rtc did not listen on {rtsp}");
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

/// Those of `pids` that are still an FFmpeg process.
pub fn still_running(pids: &[u32]) -> Vec<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    pids.iter().copied().filter(|pid| system.process(sysinfo::Pid::from_u32(*pid)).is_some_and(is_ffmpeg)).collect()
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
