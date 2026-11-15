//! Test harness: a scripted source, a real H.264 clip from FFmpeg, and a
//! str0m client playing the browser.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use oxrtc_core::frame::{AnnexBUnits, h264_is_keyframe, h264_nal_type};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, CodecParams, Declared, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind,
    ProducerSink, SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, Stream, TrackInfo,
};
use oxrtc_webrtc::{WebRtcConfig, WebRtcServer, WhepError};
use str0m::change::SdpAnswer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, MediaKind as RtcKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------- media

/// One scripted frame: track, keyframe, AVCC (or raw audio) payload, pts step.
#[derive(Clone)]
pub struct ScriptFrame {
    pub track: u8,
    pub keyframe: bool,
    pub data: Bytes,
}

#[derive(Clone)]
pub struct Script {
    pub declared: Option<Declared>,
    /// `None`: the source never announces tracks.
    pub tracks: Option<Vec<TrackInfo>>,
    pub frames: Vec<ScriptFrame>,
    pub interval: Duration,
}

fn avcc(nals: &[&[u8]]) -> Bytes {
    let mut out = BytesMut::new();
    for nal in nals {
        out.put_u32(nal.len() as u32);
        out.put_slice(nal);
    }
    out.freeze()
}

/// A synthetic H.264 GOP: no decoder is involved, only the NAL types matter.
pub fn synthetic_h264() -> (CodecParams, Vec<ScriptFrame>) {
    let sps = Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xa0, 0x47, 0xfe, 0xc8]);
    let pps = Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80]);
    let mut frames = Vec::new();
    for i in 0..25 {
        let key = i == 0;
        let mut nal = vec![if key { 0x65 } else { 0x41 }, 0x88];
        nal.extend(std::iter::repeat_n(i as u8, if key { 3000 } else { 400 }));
        frames.push(ScriptFrame { track: 0, keyframe: key, data: avcc(&[&nal]) });
    }
    (CodecParams::H264 { sps, pps }, frames)
}

/// A synthetic H.265 GOP (IDR_W_RADL then TRAIL_R).
pub fn synthetic_h265() -> (CodecParams, Vec<ScriptFrame>) {
    let params = CodecParams::H265 {
        vps: Bytes::from_static(&[0x40, 0x01, 0x0c, 0x01, 0xff, 0xff]),
        sps: Bytes::from_static(&[0x42, 0x01, 0x01, 0x01, 0x60, 0x00]),
        pps: Bytes::from_static(&[0x44, 0x01, 0xc1, 0x72]),
    };
    let mut frames = Vec::new();
    for i in 0..25 {
        let key = i == 0;
        let mut nal = vec![if key { 0x26 } else { 0x02 }, 0x01];
        nal.extend(std::iter::repeat_n(i as u8, if key { 3000 } else { 400 }));
        frames.push(ScriptFrame { track: 0, keyframe: key, data: avcc(&[&nal]) });
    }
    (params, frames)
}

pub fn ffmpeg_path() -> PathBuf {
    std::env::var_os("OXRTC_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe"))
}

/// A real two-second H.264 clip from FFmpeg, split into access units; the
/// parameter sets go to `CodecParams` (so the server has to put them back).
/// `None` only when FFmpeg is absent: once the binary exists, a failure to
/// make the clip fails the test instead of quietly testing something else.
pub async fn ffmpeg_h264() -> Option<(CodecParams, Vec<ScriptFrame>)> {
    let ffmpeg = ffmpeg_path();
    if !ffmpeg.exists() {
        eprintln!("SKIP real clip: ffmpeg not found at {} (set OXRTC_FFMPEG); using synthetic H.264", ffmpeg.display());
        return None;
    }
    let dir = std::env::temp_dir().join(format!("oxrtc-webrtc-{}-{}", std::process::id(), fastrand_u64()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("clip.h264");
    let status = tokio::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25"])
        .args(["-t", "2", "-c:v", "libx264", "-bf", "0", "-g", "25", "-pix_fmt", "yuv420p"])
        .args(["-x264-params", "aud=1", "-f", "h264"])
        .arg(&out)
        .kill_on_drop(true)
        .status()
        .await
        .unwrap_or_else(|e| panic!("ffmpeg at {} exists but did not run: {e}", ffmpeg.display()));
    let data = std::fs::read(&out);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "ffmpeg at {} exists but failed: {status}", ffmpeg.display());
    let data = data.unwrap();
    let (mut sps, mut pps) = (None, None);
    let mut frames = Vec::new();
    let mut unit: Vec<&[u8]> = Vec::new();
    let flush = |unit: &mut Vec<&[u8]>, frames: &mut Vec<ScriptFrame>| {
        if !unit.is_empty() {
            let data = avcc(unit);
            frames.push(ScriptFrame { track: 0, keyframe: h264_is_keyframe(&data), data });
            unit.clear();
        }
    };
    for nal in AnnexBUnits::new(&data) {
        match h264_nal_type(nal) {
            Some(9) => flush(&mut unit, &mut frames),
            Some(7) => sps = Some(Bytes::copy_from_slice(nal)),
            Some(8) => pps = Some(Bytes::copy_from_slice(nal)),
            _ => unit.push(nal),
        }
    }
    flush(&mut unit, &mut frames);
    let params = CodecParams::H264 { sps: sps.expect("the clip has an SPS"), pps: pps.expect("the clip has a PPS") };
    eprintln!("using a real H.264 clip from ffmpeg: {} access units", frames.len());
    assert!(frames.len() > 10 && frames[0].keyframe, "the clip starts with a keyframe and has frames");
    Some((params, frames))
}

fn fastrand_u64() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Opus silence, 20 ms frames.
pub fn opus_frames(track: u8, count: usize) -> Vec<ScriptFrame> {
    (0..count).map(|_| ScriptFrame { track, keyframe: false, data: Bytes::from_static(&[0xf8, 0xff, 0xfe]) }).collect()
}

// ---------------------------------------------------------------- source

struct FakeDriver(Script);

impl SourceDriver for FakeDriver {
    fn declared(&self) -> Option<Declared> {
        self.0.declared.clone()
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let script = &self.0;
            let Some(tracks) = script.tracks.clone() else {
                std::future::pending::<()>().await;
                return Ok(());
            };
            sink.set_tracks(tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            let mut pts = vec![0i64; tracks.len()];
            loop {
                for f in &script.frames {
                    tokio::time::sleep(script.interval).await;
                    let t = usize::from(f.track);
                    let step = if tracks[t].kind == MediaKind::Video { 3600 } else { 960 };
                    sink.publish(Frame::new(f.track, pts[t], f.keyframe, f.data.clone()))
                        .map_err(|e| SourceError::Refused(e.to_string()))?;
                    pts[t] += step;
                }
            }
        })
    }
}

#[derive(Default)]
pub struct FakeFactory {
    scripts: HashMap<String, Script>,
}

impl FakeFactory {
    pub fn with(mut self, name: &str, script: Script) -> Self {
        self.scripts.insert(name.to_owned(), script);
        self
    }
}

impl SourceFactory for FakeFactory {
    fn schemes(&self) -> &[&'static str] {
        &["fake"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let script = self.scripts.get(spec.target()).cloned().ok_or_else(|| SpecError::Invalid("no script".into()))?;
        Ok(Arc::new(FakeDriver(script)))
    }
}

pub fn hub(factory: FakeFactory) -> Arc<Hub> {
    hub_with(factory, Limits::default())
}

pub fn hub_with(factory: FakeFactory, limits: Limits) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(factory));
    Hub::new(drivers, limits)
}

/// A video track (with its parameter sets) and an Opus track, interleaved.
pub fn av_script(codec: Codec, params: CodecParams, video: Vec<ScriptFrame>, declared: bool) -> Script {
    let mut frames = Vec::new();
    let mut audio = opus_frames(1, video.len() * 2).into_iter();
    for v in video {
        frames.push(v);
        frames.extend(audio.next());
        frames.extend(audio.next());
    }
    Script {
        declared: declared.then(|| vec![(MediaKind::Video, codec), (MediaKind::Audio, Codec::Opus)]),
        tracks: Some(vec![TrackInfo::video(codec).with_params(params), TrackInfo::audio(Codec::Opus, 48_000, 2)]),
        frames,
        interval: Duration::from_millis(13),
    }
}

pub async fn server(max_sessions: usize, idle: Duration, answer_timeout: Duration) -> WebRtcServer {
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        candidates: Vec::new(),
        max_sessions,
        max_sessions_per_ip: max_sessions,
        session_idle_timeout: idle,
        answer_timeout,
    };
    WebRtcServer::bind(config, CancellationToken::new()).await.unwrap()
}

// ---------------------------------------------------------------- client

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Connected,
    /// `params`: the frame carries its parameter sets (SPS for H.264, VPS for H.265);
    /// `time`: its RTP time, in its clock; `at`: when its first packet arrived.
    Media {
        kind: RtcKind,
        codec: RtcCodec,
        keyframe: bool,
        len: usize,
        params: bool,
        time: u64,
        at: Instant,
    },
    Closed,
}

pub enum Control {
    /// Close properly (DTLS close_notify, RTCP BYE), then stop.
    Close,
    /// Stop answering, as a browser tab that crashed.
    Vanish,
}

pub struct Client {
    pub events: mpsc::Receiver<ClientEvent>,
    control: Option<oneshot::Sender<Control>>,
    pub local: SocketAddr,
}

impl Client {
    pub fn stop(&mut self, how: Control) {
        if let Some(tx) = self.control.take() {
            let _ = tx.send(how);
        }
    }

    /// Waits for an event matching `f`.
    pub async fn wait_for(&mut self, within: Duration, mut f: impl FnMut(&ClientEvent) -> bool) -> Option<ClientEvent> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) if f(&event) => return Some(event),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }
}

/// The client codec setup: H.264 (+H.265) video, Opus audio.
pub fn browser(h265: bool) -> RtcConfig {
    RtcConfig::new().clear_codecs().enable_h264(true).enable_h265(h265).enable_opus(true, false)
}

pub async fn connect(server: &WebRtcServer, stream: Arc<Stream>, config: RtcConfig) -> Result<Client, WhepError> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    connect_on(server, stream, config, socket).await
}

/// [`connect`] from `socket`, whose address is also the one the WHEP caller
/// names (as the API passes its HTTP peer).
pub async fn connect_on(
    server: &WebRtcServer,
    stream: Arc<Stream>,
    config: RtcConfig,
    socket: UdpSocket,
) -> Result<Client, WhepError> {
    let local = socket.local_addr().unwrap();
    let mut rtc = config.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let answer = server.answer(stream, &offer.to_sdp_string(), Some(local)).await?;
    assert!(answer.contains("a=candidate"), "the answer carries its candidates");
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
    let (events_tx, events) = mpsc::channel(4096);
    let (control_tx, control_rx) = oneshot::channel();
    tokio::spawn(drive(rtc, socket, events_tx, control_rx));
    Ok(Client { events, control: Some(control_tx), local })
}

async fn drive(
    mut rtc: Rtc,
    socket: UdpSocket,
    events: mpsc::Sender<ClientEvent>,
    mut control: oneshot::Receiver<Control>,
) {
    let local = socket.local_addr().unwrap();
    let mut buf = vec![0u8; 2048];
    loop {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(event)) => {
                    let mapped = match event {
                        Event::Connected => Some(ClientEvent::Connected),
                        Event::MediaData(data) => Some(ClientEvent::Media {
                            kind: if data.params.spec().codec.is_video() { RtcKind::Video } else { RtcKind::Audio },
                            codec: data.params.spec().codec,
                            keyframe: data.is_keyframe(),
                            len: data.data.len(),
                            params: carries_parameter_sets(data.params.spec().codec, &data.data),
                            time: data.time.numer(),
                            at: data.network_time,
                        }),
                        Event::Closed => Some(ClientEvent::Closed),
                        _ => None,
                    };
                    if let Some(e) = mapped {
                        let _ = events.try_send(e);
                    }
                }
                Err(_) => return,
            }
        };
        if !rtc.is_alive() {
            return;
        }
        let wait = tokio::time::Instant::from_std(timeout.max(Instant::now()));
        tokio::select! {
            how = &mut control => {
                match how {
                    Ok(Control::Close) => {
                        let _ = rtc.close();
                        for _ in 0..64 {
                            match rtc.poll_output() {
                                Ok(Output::Transmit(t)) => { let _ = socket.send_to(&t.contents, t.destination).await; }
                                Ok(Output::Event(_)) => {}
                                _ => break,
                            }
                        }
                    }
                    Ok(Control::Vanish) | Err(_) => {}
                }
                return;
            }
            received = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = received else { continue };
                let Ok(contents) = buf[..n].try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                if rtc.handle_input(input).is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(wait) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                    return;
                }
            }
        }
    }
}

fn carries_parameter_sets(codec: RtcCodec, annex_b: &[u8]) -> bool {
    AnnexBUnits::new(annex_b).any(|nal| match codec {
        RtcCodec::H264 => h264_nal_type(nal) == Some(7),
        RtcCodec::H265 => oxrtc_core::frame::h265_nal_type(nal) == Some(32),
        _ => false,
    })
}

/// Polls `f` until it holds or `within` passes.
pub async fn eventually(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}
