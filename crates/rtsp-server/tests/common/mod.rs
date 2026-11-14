//! Shared harness: a hub with scripted sources, the server on a free port, a
//! raw RTSP client, and the external FFmpeg binaries (skipped when absent).
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::frame::{
    AnnexBUnits, annex_b_to_avcc, h264_is_keyframe, h264_nal_type, h265_is_keyframe, h265_nal_type,
};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, CodecParams, Declared, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind,
    ProducerSink, SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};
use oxrtc_rtsp_server::{RtspServer, RtspServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const FFMPEG_DEFAULT: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
pub const FFPROBE_DEFAULT: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffprobe.exe";

fn binary(var: &str, default: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_owned()));
    if path.is_file() {
        Some(path)
    } else {
        eprintln!("SKIP: {} not found (set {var})", path.display());
        None
    }
}

pub fn ffmpeg() -> Option<PathBuf> {
    binary("OXRTC_FFMPEG", FFMPEG_DEFAULT)
}

pub fn ffprobe() -> Option<PathBuf> {
    binary("OXRTC_FFPROBE", FFPROBE_DEFAULT)
}

/// A clip: tracks and frames (track, keyframe, payload), replayed in a loop.
#[derive(Clone)]
pub struct Clip {
    pub tracks: Vec<TrackInfo>,
    pub frames: Vec<(u8, bool, Bytes)>,
    /// Video frames per second (audio frames ride along).
    pub fps: u32,
}

impl Clip {
    /// A synthetic clip: tiny fake H.264 frames (GOP of 25) and PCMA audio.
    pub fn synthetic() -> Self {
        let sps = Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f, 0xac]);
        let pps = Bytes::from_static(&[0x68, 0xee, 0x3c, 0xb0]);
        let mut frames = Vec::new();
        for i in 0..25 {
            let data: &'static [u8] = if i == 0 { &[0, 0, 0, 2, 0x65, 0x88] } else { &[0, 0, 0, 2, 0x41, 0x9a] };
            frames.push((0, i == 0, Bytes::from_static(data)));
            frames.push((1, true, Bytes::from_static(&[0xd5; 320])));
        }
        Self {
            tracks: vec![
                TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps, pps }),
                TrackInfo::audio(Codec::Pcma, 8000, 1),
            ],
            frames,
            fps: 25,
        }
    }

    /// A real clip from an Annex B elementary stream with access unit
    /// delimiters. Parameter sets are moved to the track's `CodecParams`, so
    /// the server must insert them before each keyframe.
    pub fn from_annex_b(codec: Codec, data: &[u8]) -> Self {
        type NalType = fn(&[u8]) -> Option<u8>;
        let (aud, kind): (u8, NalType) = match codec {
            Codec::H264 => (9, h264_nal_type),
            _ => (35, h265_nal_type),
        };
        let mut sets: HashMap<u8, Bytes> = HashMap::new();
        let mut units: Vec<Vec<&[u8]>> = Vec::new();
        for nal in AnnexBUnits::new(data) {
            let t = kind(nal).unwrap();
            if t == aud {
                units.push(Vec::new());
            } else if matches!((codec, t), (Codec::H264, 7 | 8) | (Codec::H265, 32..=34)) {
                sets.entry(t).or_insert_with(|| Bytes::copy_from_slice(nal));
            } else if let Some(unit) = units.last_mut() {
                unit.push(nal);
            }
        }
        let frames = units
            .into_iter()
            .filter(|u| !u.is_empty())
            .map(|u| {
                let mut annex_b = Vec::new();
                for nal in u {
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(nal);
                }
                let avcc = annex_b_to_avcc(&annex_b).unwrap();
                let key = if codec == Codec::H264 { h264_is_keyframe(&avcc) } else { h265_is_keyframe(&avcc) };
                (0u8, key, avcc)
            })
            .collect();
        let params = match codec {
            Codec::H264 => CodecParams::H264 { sps: sets[&7].clone(), pps: sets[&8].clone() },
            _ => CodecParams::H265 { vps: sets[&32].clone(), sps: sets[&33].clone(), pps: sets[&34].clone() },
        };
        Self { tracks: vec![TrackInfo::video(codec).with_params(params)], frames, fps: 25 }
    }
}

struct ClipDriver(Arc<Clip>);

impl SourceDriver for ClipDriver {
    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let clip = &self.0;
            sink.set_tracks(clip.tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            let interval = Duration::from_secs(1) / clip.fps;
            let mut ticker = tokio::time::interval(interval);
            let (mut video_n, mut audio_n) = (0i64, 0i64);
            loop {
                for (track, key, data) in &clip.frames {
                    let info = &clip.tracks[usize::from(*track)];
                    let pts = if info.kind == MediaKind::Video {
                        tokio::select! {
                            () = cancel.cancelled() => return Ok(()),
                            _ = ticker.tick() => {}
                        }
                        video_n += 1;
                        (video_n - 1) * i64::from(90_000 / clip.fps)
                    } else {
                        audio_n += 1;
                        (audio_n - 1) * data.len() as i64
                    };
                    sink.publish(Frame::new(*track, pts, *key, data.clone()))
                        .map_err(|e| SourceError::Refused(e.to_string()))?;
                }
            }
        })
    }
}

/// Hands each run's sink to the test, then holds until cancelled.
struct HandoffDriver(mpsc::Sender<ProducerSink>, Declared);

impl SourceDriver for HandoffDriver {
    fn declared(&self) -> Option<Declared> {
        Some(self.1.clone())
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let _ = self.0.send(sink).await;
            cancel.cancelled().await;
            Ok(())
        })
    }
}

/// `clip:<name>` plays a registered clip; `handoff:<name>` hands its sink over.
#[derive(Default)]
pub struct Factory {
    clips: Mutex<HashMap<String, Arc<Clip>>>,
    handoffs: Mutex<HashMap<String, (mpsc::Sender<ProducerSink>, Declared)>>,
}

impl Factory {
    pub fn clip(&self, name: &str, clip: Clip) {
        self.clips.lock().unwrap().insert(name.to_owned(), Arc::new(clip));
    }

    /// A source declaring `declared` that hands its sink over.
    pub fn handoff(&self, name: &str, declared: Declared) -> mpsc::Receiver<ProducerSink> {
        let (tx, rx) = mpsc::channel(4);
        self.handoffs.lock().unwrap().insert(name.to_owned(), (tx, declared));
        rx
    }
}

impl SourceFactory for Factory {
    fn schemes(&self) -> &[&'static str] {
        &["clip", "handoff"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let missing = || SpecError::Invalid("no such test source".into());
        match spec.scheme() {
            "clip" => {
                Ok(Arc::new(ClipDriver(self.clips.lock().unwrap().get(spec.target()).cloned().ok_or_else(missing)?)))
            }
            _ => {
                let (tx, declared) = self.handoffs.lock().unwrap().get(spec.target()).cloned().ok_or_else(missing)?;
                Ok(Arc::new(HandoffDriver(tx, declared)))
            }
        }
    }
}

pub fn hub(factory: Arc<Factory>) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(factory);
    Hub::new(
        drivers,
        Limits {
            idle_grace: Duration::from_millis(300),
            probe_timeout: Duration::from_secs(3),
            connect_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_secs(1),
            ..Limits::default()
        },
    )
}

pub fn config() -> RtspServerConfig {
    RtspServerConfig { listen: SocketAddr::from(([127, 0, 0, 1], 0)), ..RtspServerConfig::default() }
}

pub struct Running {
    pub addr: SocketAddr,
    pub publishers: oxrtc_rtsp_server::Publishers,
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    pub fn url(&self, path: &str) -> String {
        format!("rtsp://{}/{path}", self.addr)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub async fn start(hub: Arc<Hub>, config: RtspServerConfig) -> Running {
    let server = RtspServer::bind(config, hub).await.unwrap();
    let addr = server.local_addr();
    let publishers = server.publishers();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(server.run(cancel.clone()));
    Running { addr, publishers, cancel, task }
}

/// A minimal RTSP client over one TCP connection.
pub struct Client {
    pub stream: TcpStream,
    cseq: u32,
    buf: Vec<u8>,
}

#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub head: String,
    pub body: String,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Self {
        Self { stream: TcpStream::connect(addr).await.unwrap(), cseq: 0, buf: Vec::new() }
    }

    /// Sends `method url` with extra header lines and an optional body.
    pub async fn request(&mut self, method: &str, url: &str, headers: &[&str], body: &str) -> Option<Answer> {
        self.cseq += 1;
        let mut text = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for h in headers {
            text.push_str(h);
            text.push_str("\r\n");
        }
        if !body.is_empty() {
            text.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        text.push_str("\r\n");
        text.push_str(body);
        self.stream.write_all(text.as_bytes()).await.ok()?;
        self.answer().await
    }

    /// Reads one response, skipping interleaved frames. `None` on close.
    pub async fn answer(&mut self) -> Option<Answer> {
        loop {
            while self.buf.first() == Some(&b'$') && self.buf.len() >= 4 {
                let len = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
                if self.buf.len() < 4 + len {
                    break;
                }
                self.buf.drain(..4 + len);
            }
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n")
                && self.buf.first() != Some(&b'$')
            {
                let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if self.buf.len() >= end + 4 + len {
                    let body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + len]).into_owned();
                    self.buf.drain(..end + 4 + len);
                    let status = head.split_whitespace().nth(1)?.parse().ok()?;
                    return Some(Answer { status, head, body });
                }
            }
            let mut chunk = [0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(15), self.stream.read(&mut chunk)).await.ok()?.ok()?;
            if n == 0 {
                return None;
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Reads until the server closes, or `wait` passes. `true`: closed.
    pub async fn closed_within(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        let mut chunk = [0u8; 65536];
        loop {
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Err(_) => return false,
                Ok(Ok(0) | Err(_)) => return true,
                Ok(Ok(_)) => {}
            }
        }
    }
}

/// Runs a binary with a deadline, returning (success, stdout, stderr).
pub async fn run(program: &std::path::Path, args: &[&str], deadline: Duration) -> (bool, String, String) {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = tokio::time::timeout(deadline, child.wait_with_output()).await.expect("binary timed out").unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Encodes `seconds` of test pattern to an Annex B file with access unit
/// delimiters, and returns its bytes.
pub async fn encode(ffmpeg: &std::path::Path, codec: Codec, seconds: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let (encoder, bsf, format, extra): (&str, &str, &str, &[&str]) = match codec {
        Codec::H264 => ("libx264", "h264_metadata=aud=insert", "h264", &["-bf", "0"]),
        _ => ("libx265", "hevc_metadata=aud=insert", "hevc", &["-x265-params", "log-level=error:bframes=0"]),
    };
    let out = dir.path().join(format!("clip.{format}"));
    let out_s = out.to_str().unwrap().to_owned();
    let secs = seconds.to_string();
    let mut args = vec![
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=25",
        "-t",
        &secs,
        "-c:v",
        encoder,
        "-g",
        "25",
        "-pix_fmt",
        "yuv420p",
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["-bsf:v", bsf, "-f", format, "-y", &out_s]);
    let (ok, _, err) = run(ffmpeg, &args, Duration::from_secs(60)).await;
    assert!(ok, "encoding failed: {err}");
    tokio::fs::read(&out).await.unwrap()
}
