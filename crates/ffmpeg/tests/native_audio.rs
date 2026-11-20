//! The native sound transcode (W5, D37): `ffmpeg:` sources of G.711 made
//! into Opus or G.711 in process. Most of these run with an FFmpeg that does
//! not exist, so a source that started one would fail; the last ones run the
//! real FFmpeg for an input the native reader leaves to it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::{Clip, Harness, ffmpeg, retry_reason};
use oxrtc_core::{BoxFuture, Codec, ConsumerTag, Event, MediaKind, SourceError, SourceState, Want};
use oxrtc_ffmpeg::{AudioHttp, HttpBody};
use oxrtc_http_client::{Bodies, Body, HttpClientConfig, HttpFactory};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

/// An FFmpeg that is not there: a source that starts one fails.
fn no_ffmpeg() -> PathBuf {
    PathBuf::from("no-such-ffmpeg-w5")
}

/// The webcams' HTTP client, as the binary hands it over.
struct Http(Bodies);
struct HttpAnswer(Body);

impl HttpBody for HttpAnswer {
    fn status(&self) -> u16 {
        self.0.status()
    }
    fn content_type(&self) -> Option<&str> {
        self.0.content_type()
    }
    fn chunk(&mut self) -> BoxFuture<'_, Option<Result<Bytes, SourceError>>> {
        Box::pin(self.0.chunk())
    }
}

impl AudioHttp for Http {
    fn get(&self, url: String, timeout: Duration) -> BoxFuture<'static, Result<Box<dyn HttpBody>, SourceError>> {
        let bodies = self.0.clone();
        Box::pin(async move { Ok(Box::new(HttpAnswer(bodies.get(&url, timeout).await?)) as Box<dyn HttpBody>) })
    }
}

fn http() -> Option<Arc<dyn AudioHttp>> {
    Some(Arc::new(Http(HttpFactory::new(HttpClientConfig::default()).bodies())))
}

/// openIVR's `dedup_input` template.
const DEDUP_INPUT: (&str, &str) = ("dedup_input", "-fflags nobuffer -flags low_delay -probesize 32 -i {input}");

/// openIVR's sound de-duplicator as oxrtc sees it (`audio_dedup.py`): an
/// HTTP/1.0 answer `status` with, for a 200, an endless WAV of `header`
/// then 20 ms of samples of `sample` bytes every 20 ms.
async fn wav_server(status: &'static str, format: [u8; 16], sample: &'static [u8], per_20ms: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let mut out = format!("HTTP/1.0 {status}\r\n").into_bytes();
                if !status.starts_with("200") {
                    out.extend_from_slice(b"Content-Length: 0\r\n\r\n");
                    let _ = socket.write_all(&out).await;
                    return;
                }
                out.extend_from_slice(b"Content-Type: audio/wav\r\nConnection: close\r\n\r\n");
                out.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00");
                out.extend_from_slice(&format);
                out.extend_from_slice(b"data\xff\xff\xff\xff");
                let _ = socket.write_all(&out).await;
                let packet: Vec<u8> = sample.iter().copied().cycle().take(per_20ms * sample.len()).collect();
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if socket.write_all(&packet).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}/camera_1_src")
}

/// A-law, mono, 16 kHz, 8 bits: what the de-duplicator serves for the owner's cameras.
const ALAW_16K: [u8; 16] = [6, 0, 1, 0, 0x80, 0x3e, 0, 0, 0x80, 0x3e, 0, 0, 1, 0, 8, 0];
/// PCM of 24 bits, mono, 16 kHz: a WAV the native reader leaves to FFmpeg.
const PCM24_16K: [u8; 16] = [1, 0, 1, 0, 0x80, 0x3e, 0, 0, 0x80, 0xbb, 0, 0, 3, 0, 24, 0];

/// The frames of `stream` read with `want` for as long as `wanted` take,
/// with the track list they came under.
async fn frames(
    stream: &Arc<oxrtc_core::Stream>,
    want: &str,
    wanted: usize,
) -> (Vec<oxrtc_core::TrackInfo>, Vec<Arc<oxrtc_core::Frame>>) {
    let mut sub = stream.subscribe(&Want::from_query(want).unwrap(), ConsumerTag::new("test")).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut tracks, mut got) = (Vec::new(), Vec::new());
    while got.len() < wanted {
        match tokio::time::timeout_at(deadline, sub.recv()).await.expect("too few frames").unwrap() {
            Event::Tracks(list) => tracks = list.to_vec(),
            Event::Frame { frame, .. } => got.push(frame),
            Event::Discontinuity => {}
        }
    }
    (tracks, got)
}

/// openIVR's plain sound source (`ffmpeg:rtsp://<relay>/camera_N_src#audio=opus`):
/// Opus at 48000, a packet of 20 ms, with no FFmpeg at all.
#[tokio::test(flavor = "multi_thread")]
async fn g711_of_a_relay_url_is_opus_with_no_ffmpeg() {
    let h = Harness::start_native(&no_ffmpeg(), vec![("mic", Clip::pcma())], &[], None).await;
    h.stream("mic_src", &["clip:mic"]);
    let spec = format!("ffmpeg:rtsp://{}/mic_src#audio=opus#timeout=20", h.addr);
    let mic = h.stream("mic", &[spec.as_str()]);
    let (tracks, got) = frames(&mic, "audio=opus", 50).await;
    assert_eq!(
        (tracks[0].kind, tracks[0].codec, tracks[0].clock_rate, tracks[0].channels),
        (MediaKind::Audio, Codec::Opus, 48_000, 2)
    );
    assert!(got.windows(2).all(|w| w[1].pts - w[0].pts == 960), "20 ms a packet");
    assert!(got.iter().all(|f| !f.data.is_empty() && f.data.len() < 400));
    // The stream lists one consumer more on the camera: the transcode.
    let camera = h.hub.get("mic_src").unwrap().snapshot();
    assert!(camera.consumers.iter().any(|c| c.kind == "native_audio"), "{:?}", camera.consumers);
    assert!(!matches!(mic.snapshot().sources[0].state, SourceState::Retrying { .. }));
}

/// By name, to G.711 mu-law: each sample the A-law one, coded again.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_by_name_is_g711_of_the_other_law() {
    let h = Harness::start_native(&no_ffmpeg(), vec![("mic", Clip::pcma())], &[], None).await;
    h.stream("mic_src", &["clip:mic"]);
    let mic = h.stream("mic", &["ffmpeg:mic_src#audio=pcmu"]);
    let (tracks, got) = frames(&mic, "audio", 20).await;
    assert_eq!((tracks[0].codec, tracks[0].clock_rate), (Codec::Pcmu, 8000));
    let clip = Clip::pcma().frames[0].1.clone();
    let expected: Vec<u8> =
        clip.iter().map(|&c| oxrtc_audio::g711::encode_ulaw(oxrtc_audio::g711::decode_alaw(c))).collect();
    assert!(got.iter().all(|f| f.data[..] == expected[..]), "{:02x?}", &got[0].data[..8]);
}

/// The sound keeps its source's clock (D12): each packet is received when its
/// input frame was, and dated as far from it as every other packet.
#[tokio::test(flavor = "multi_thread")]
async fn the_sound_keeps_its_sources_clock() {
    let h = Harness::start_native(&no_ffmpeg(), vec![("mic", Clip::pcma())], &[], None).await;
    let source = h.stream("mic_src", &["clip:mic"]);
    let mic = h.stream("mic", &["ffmpeg:mic_src#audio=opus"]);
    let (input, output) = tokio::join!(frames(&source, "audio", 120), frames(&mic, "audio", 60));
    let mut offsets = Vec::new();
    for out in &output.1 {
        let Some(inp) = input.1.iter().find(|f| f.received == out.received) else { continue };
        offsets.push(out.pts as f64 / 48.0 - inp.pts as f64 / 8.0);
    }
    assert!(offsets.len() >= 40, "{} packets matched their input frame", offsets.len());
    let (low, high) = offsets.iter().fold((f64::MAX, f64::MIN), |(l, h), &o| (l.min(o), h.max(o)));
    eprintln!("packet date minus its input's: {low:.3} to {high:.3} ms");
    assert!(high - low < 1.0, "{low} to {high} ms");
}

/// No packet reaches a reader before the instant it says it was received.
/// The owner's XM cameras send 64 ms of A-law at 16 kHz a frame, cut into
/// 20 ms of Opus, each packet received when its first sample was (the
/// frame's arrival moved by that sample's place in it, D37); published at
/// once, two packets in three were read up to 44 ms before their own
/// instant, and str0m's sender report written after such a frame says RTP
/// time 0 (its wall clock in the future): on RC1 the browser placed the sound
/// seconds, once 107 s, from its picture, and held the picture up to 7 s (W5,
/// the session of 2026-11-20). A packet now waits for its instant; its date
/// and its arrival are what they were, and none is lost.
#[tokio::test(flavor = "multi_thread")]
async fn no_packet_is_received_in_the_future() {
    let h = Harness::start_native(&no_ffmpeg(), vec![("mic", Clip::pcma_xm())], &[], None).await;
    h.stream("mic_src", &["clip:mic"]);
    let mic = h.stream("mic", &["ffmpeg:mic_src#audio=opus"]);
    let mut sub = mic.subscribe(&Want::from_query("audio").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let (mut got, mut ahead) = (Vec::new(), Duration::ZERO);
    while got.len() < 100 {
        let event = tokio::time::timeout(Duration::from_secs(20), sub.recv()).await.expect("too few").unwrap();
        if let Event::Frame { frame, .. } = event {
            ahead = ahead.max(frame.received.saturating_duration_since(Instant::now()));
            got.push(frame);
        }
    }
    assert_eq!(ahead, Duration::ZERO, "a packet received {ahead:?} after it was read");
    assert!(got.windows(2).all(|w| w[1].pts - w[0].pts == 960), "a packet lost or out of order");
    assert!(got.windows(2).all(|w| w[1].received >= w[0].received), "the packets' instants go back");
}

/// openIVR's de-duplicated sound: a WAV over HTTP, read with no FFmpeg.
#[tokio::test(flavor = "multi_thread")]
async fn the_deduplicators_wav_is_opus_with_no_ffmpeg() {
    let url = wav_server("200 OK", ALAW_16K, &[0xd5, 0x55, 0xa5], 320).await;
    let h = Harness::start_native(&no_ffmpeg(), vec![], &[DEDUP_INPUT], http()).await;
    let spec = format!("ffmpeg:{url}#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let (tracks, got) = frames(&sound, "audio=opus", 50).await;
    assert_eq!((tracks[0].codec, tracks[0].clock_rate), (Codec::Opus, 48_000));
    assert!(got.windows(2).all(|w| w[1].pts - w[0].pts == 960));
}

/// A `415` of the de-duplicator is still a decline, as FFmpeg's was: the
/// stream falls through to its next source, and the source backs off slowly.
#[tokio::test(flavor = "multi_thread")]
async fn a_deduplicator_that_answers_415_declines_the_run() {
    let url = wav_server("415 Unsupported Media Type", ALAW_16K, &[0xd5], 1).await;
    let h = Harness::start_native(&no_ffmpeg(), vec![], &[DEDUP_INPUT], http()).await;
    let spec = format!("ffmpeg:{url}#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let _sub = sound.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await;
    let why = retry_reason(&sound, Duration::from_secs(20)).await;
    assert!(why.starts_with("declined: native audio: Server returned 415"), "{why}");
}

/// A stream with no sound declines, as the relay's 455 did.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_without_sound_declines() {
    let picture = Clip::jpeg(Bytes::from_static(b"\xff\xd8\xff\xd9"));
    let h = Harness::start_native(&no_ffmpeg(), vec![("cam", picture)], &[], None).await;
    h.stream("cam_src", &["clip:cam"]);
    let sound = h.stream("cam_rec", &["ffmpeg:cam_src#audio=opus"]);
    let _sub = sound.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await;
    let why = retry_reason(&sound, Duration::from_secs(20)).await;
    assert!(why.starts_with("declined: native audio"), "{why}");
}

/// A WAV the native reader does not take (24-bit samples) is FFmpeg's, in
/// the same run: with no FFmpeg it is FFmpeg's failure...
#[tokio::test(flavor = "multi_thread")]
async fn a_wav_the_reader_does_not_take_is_left_to_ffmpeg() {
    let url = wav_server("200 OK", PCM24_16K, &[0, 0x10, 0], 320).await;
    let h = Harness::start_native(&no_ffmpeg(), vec![], &[DEDUP_INPUT], http()).await;
    let spec = format!("ffmpeg:{url}#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let _sub = sound.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await;
    let why = retry_reason(&sound, Duration::from_secs(20)).await;
    assert!(why.contains("ffmpeg"), "{why}");
    drop(h);
    // ... and with FFmpeg, its Opus.
    let Some(bin) = ffmpeg() else { return };
    let h = Harness::start_native(&bin, vec![], &[DEDUP_INPUT], http()).await;
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let (tracks, got) = frames(&sound, "audio=opus", 20).await;
    assert_eq!(tracks[0].codec, Codec::Opus);
    assert_eq!(got.len(), 20);
}

/// `ffmpeg.native_audio: false`: the same source is FFmpeg's, as before W5.
#[tokio::test(flavor = "multi_thread")]
async fn with_native_audio_off_ffmpeg_makes_the_sound() {
    let h = Harness::start(&no_ffmpeg(), vec![("mic", Clip::pcma())], &[], 4, Duration::from_secs(20), None).await;
    assert!(!h.config.native_audio);
    h.stream("mic_src", &["clip:mic"]);
    let mic = h.stream("mic", &["ffmpeg:mic_src#audio=opus"]);
    let _sub = mic.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await;
    let why = retry_reason(&mic, Duration::from_secs(20)).await;
    assert!(why.contains("ffmpeg"), "{why}");
}
