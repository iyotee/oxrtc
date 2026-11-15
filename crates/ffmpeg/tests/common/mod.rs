//! Shared harness: a hub whose scripted sources are served by a real RTSP
//! server, an `ffmpeg:` factory publishing back into that server, and the
//! external FFmpeg binaries (tests skip when they are absent).
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::frame::{
    AnnexBUnits, annex_b_to_avcc, h264_is_keyframe, h264_nal_type, h265_is_keyframe, h265_nal_type,
};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, CodecParams, Drivers, FactoryCtx, Frame, Hub, Limits, ProducerSink,
    SourceDriver, SourceError, SourceFactory, SourceSpec, SourceState, SpecError, Stream, TrackInfo,
};
use oxrtc_ffmpeg::{FfmpegConfig, FfmpegFactory, PublishLease, PublishTarget};
use oxrtc_rtsp_server::{PublishHandle, Publishers, RtspServer, RtspServerConfig};

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

/// A launcher that starts the real FFmpeg as its own child: Chocolatey's
/// `ffmpeg.exe` shim, what a bare `ffmpeg` resolves to on such a machine.
pub fn launcher() -> Option<PathBuf> {
    if cfg!(windows) { binary("OXRTC_FFMPEG_LAUNCHER", "C:/ProgramData/chocolatey/bin/ffmpeg.exe") } else { None }
}

/// One track replayed in a loop: a frame every `interval`, `pts_step` apart.
#[derive(Clone)]
pub struct Clip {
    pub track: TrackInfo,
    pub frames: Vec<(bool, Bytes)>,
    pub interval: Duration,
    pub pts_step: i64,
}

impl Clip {
    /// Annex B with access unit delimiters, parameter sets moved to the
    /// track's `CodecParams` (as a camera announces them in its SDP).
    pub fn from_annex_b(codec: Codec, data: &[u8]) -> Self {
        type NalType = fn(&[u8]) -> Option<u8>;
        let (aud, kind): (u8, NalType) = if codec == Codec::H264 { (9, h264_nal_type) } else { (35, h265_nal_type) };
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
                (key, avcc)
            })
            .collect();
        let params = match codec {
            Codec::H264 => CodecParams::H264 { sps: sets[&7].clone(), pps: sets[&8].clone() },
            _ => CodecParams::H265 { vps: sets[&32].clone(), sps: sets[&33].clone(), pps: sets[&34].clone() },
        };
        Self {
            track: TrackInfo::video(codec).with_params(params),
            frames,
            interval: Duration::from_millis(40),
            pts_step: 3600,
        }
    }

    /// G.711 A-law at 8 kHz: a 400 Hz tone, 20 ms per frame.
    pub fn pcma() -> Self {
        let frame: Vec<u8> = (0..160).map(|i| if (i / 10) % 2 == 0 { 0xa5 } else { 0x25 }).collect();
        Self {
            track: TrackInfo::audio(Codec::Pcma, 8000, 1),
            frames: vec![(true, Bytes::from(frame))],
            interval: Duration::from_millis(20),
            pts_step: 160,
        }
    }

    /// One Motion JPEG frame, repeated.
    pub fn jpeg(data: Bytes) -> Self {
        Self {
            track: TrackInfo::video(Codec::Jpeg),
            frames: vec![(true, data)],
            interval: Duration::from_millis(40),
            pts_step: 3600,
        }
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
            sink.set_tracks(vec![clip.track.clone()]).map_err(|e| SourceError::Refused(e.to_string()))?;
            let mut ticker = tokio::time::interval(clip.interval);
            let mut pts = 0i64;
            loop {
                for (key, data) in &clip.frames {
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(()),
                        _ = ticker.tick() => {}
                    }
                    sink.publish(Frame::new(0, pts, *key, data.clone()))
                        .map_err(|e| SourceError::Refused(e.to_string()))?;
                    pts += clip.pts_step;
                }
            }
        })
    }
}

/// `clip:<name>` plays a registered clip.
#[derive(Default)]
pub struct Clips(Mutex<HashMap<String, Arc<Clip>>>);

impl SourceFactory for Clips {
    fn schemes(&self) -> &[&'static str] {
        &["clip"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let clip = self.0.lock().unwrap().get(spec.target()).cloned();
        Ok(Arc::new(ClipDriver(clip.ok_or_else(|| SpecError::Invalid("no such clip".into()))?)))
    }
}

/// The `ffmpeg:` factory, built once the server it publishes into is bound
/// (the server needs the hub, the hub needs the factory).
#[derive(Default)]
struct LateFfmpeg(OnceLock<FfmpegFactory>);

impl SourceFactory for LateFfmpeg {
    fn schemes(&self) -> &[&'static str] {
        &["ffmpeg"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        self.0.get().ok_or_else(|| SpecError::Invalid("factory not ready".into()))?.build(spec, ctx)
    }
}

/// What the binary will do: publication tokens of the RTSP server.
pub struct ServerTarget(pub Publishers);

struct ServerLease(PublishHandle);

impl PublishLease for ServerLease {
    fn url(&self) -> String {
        self.0.url()
    }
}

impl PublishTarget for ServerTarget {
    fn lease(&self, sink: ProducerSink) -> Box<dyn PublishLease> {
        Box::new(ServerLease(self.0.register(sink)))
    }
}

/// A target that hands out one fixed URL and ignores the sink.
pub struct FixedTarget(pub String);

struct FixedLease(String);

impl PublishLease for FixedLease {
    fn url(&self) -> String {
        self.0.clone()
    }
}

impl PublishTarget for FixedTarget {
    fn lease(&self, _sink: ProducerSink) -> Box<dyn PublishLease> {
        Box::new(FixedLease(self.0.clone()))
    }
}

pub struct Harness {
    pub hub: Arc<Hub>,
    pub addr: SocketAddr,
    pub config: FfmpegConfig,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.hub.shutdown();
        self.cancel.cancel();
    }
}

impl Harness {
    /// A hub with `clips` as `clip:<name>` sources and the `ffmpeg:` scheme,
    /// served on a free port. `target` replaces the server's own tokens.
    pub async fn start(
        bin: &Path,
        clips: Vec<(&str, Clip)>,
        templates: &[(&str, &str)],
        max_processes: usize,
        start_timeout: Duration,
        target: Option<Arc<dyn PublishTarget>>,
    ) -> Self {
        let factory = Arc::new(Clips::default());
        for (name, clip) in clips {
            factory.0.lock().unwrap().insert(name.to_owned(), Arc::new(clip));
        }
        let late = Arc::new(LateFfmpeg::default());
        let mut drivers = Drivers::new();
        drivers.register(factory);
        drivers.register(late.clone());
        let limits = Limits {
            idle_grace: Duration::from_millis(300),
            connect_timeout: Duration::from_secs(20),
            stall_timeout: Duration::from_secs(10),
            backoff_initial: Duration::from_millis(200),
            backoff_max: Duration::from_secs(2),
            ..Limits::default()
        };
        let hub = Hub::new(drivers, limits);
        let listen = SocketAddr::from(([127, 0, 0, 1], 0));
        let server =
            RtspServer::bind(RtspServerConfig { listen, ..RtspServerConfig::default() }, hub.clone()).await.unwrap();
        let addr = server.local_addr();
        let config = FfmpegConfig {
            bin: bin.to_owned(),
            templates: templates.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<BTreeMap<_, _>>(),
            relay: addr.to_string(),
            max_processes,
            start_timeout,
            confine: confinement(),
        };
        let target = target.unwrap_or_else(|| Arc::new(ServerTarget(server.publishers())));
        assert!(late.0.set(FfmpegFactory::new(config.clone(), target)).is_ok());
        let cancel = CancellationToken::new();
        tokio::spawn(server.run(cancel.clone()));
        Self { hub, addr, config, cancel }
    }

    pub fn stream(&self, name: &str, sources: &[&str]) -> Arc<Stream> {
        self.hub.upsert(name, sources).unwrap()
    }
}

/// Waits until the first source of `stream` is retrying, and returns why.
pub async fn retry_reason(stream: &Stream, within: Duration) -> String {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(SourceState::Retrying { error, .. }) = stream.snapshot().sources.first().map(|s| s.state.clone()) {
            return error;
        }
        assert!(tokio::time::Instant::now() < deadline, "never retried: {:?}", stream.snapshot().sources);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runs a binary with a deadline: (success, stdout, stderr).
pub async fn run(program: &Path, args: &[&str], deadline: Duration) -> (bool, String, String) {
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

/// `seconds` of test pattern at `size`, as Annex B with access unit delimiters.
pub async fn encode(ffmpeg: &Path, codec: Codec, size: &str, seconds: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let (encoder, bsf, format, extra): (&str, &str, &str, &[&str]) = match codec {
        Codec::H264 => ("libx264", "h264_metadata=aud=insert", "h264", &["-bf", "0"]),
        _ => ("libx265", "hevc_metadata=aud=insert", "hevc", &["-x265-params", "log-level=error:bframes=0"]),
    };
    let out = dir.path().join(format!("clip.{format}"));
    let out_s = out.to_str().unwrap().to_owned();
    let source = format!("testsrc2=size={size}:rate=25");
    let secs = seconds.to_string();
    let mut args = vec![
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        &source,
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

/// Width and height from a JPEG's start-of-frame marker.
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u16, u16)> {
    let mut at = 2;
    while at + 9 < jpeg.len() {
        if jpeg[at] != 0xff {
            return None;
        }
        let marker = jpeg[at + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if matches!(marker, 0xc0..=0xc3) {
            let height = u16::from_be_bytes([jpeg[at + 5], jpeg[at + 6]]);
            let width = u16::from_be_bytes([jpeg[at + 7], jpeg[at + 8]]);
            return Some((width, height));
        }
        at += 2 + len;
    }
    None
}

/// On Linux, FFmpeg runs confined as oxrtc runs it, through the `oxrtc-exec`
/// that `cargo test --workspace` builds (`target/<profile>/`): every transcode
/// test is then also a test of FFmpeg under the confinement. Elsewhere, and
/// when only this crate is built, directly.
pub fn confinement() -> Option<oxrtc_ffmpeg::Confinement> {
    let exe = std::env::current_exe().ok()?;
    let beside_oxrtc = exe.parent()?.parent()?.join("oxrtc");
    match oxrtc_ffmpeg::Confinement::discover(&beside_oxrtc) {
        oxrtc_ffmpeg::Discovery::Found(confinement) => Some(confinement),
        oxrtc_ffmpeg::Discovery::NotHere => None,
        oxrtc_ffmpeg::Discovery::Missing(path) => {
            eprintln!("NOTE: {} not built, FFmpeg runs unconfined (cargo test --workspace builds it)", path.display());
            None
        }
    }
}
