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
    packets_on_time(Clip::pcma_xm()).await;
}

/// The same when the frames come two at once (a relay or a camera that sends
/// its sound in pairs, or a source's timer that fell behind and caught up):
/// the next frame's first packet starts in the samples the frame before left
/// over, and was placed back from the frame's arrival, before the instant of
/// that frame's last packet - its instants went back (every 128 ms here; one
/// run in twenty of the test above under a loaded machine, where the clip's
/// timer catches up). A packet's instant is never before the one before it.
#[tokio::test(flavor = "multi_thread")]
async fn no_packet_goes_back_when_frames_come_in_pairs() {
    packets_on_time(Clip::pcma_xm_pairs()).await;
}

/// 100 Opus packets of `clip`'s sound: none read before its instant, none
/// lost, and their instants in order.
async fn packets_on_time(clip: Clip) {
    let h = Harness::start_native(&no_ffmpeg(), vec![("mic", clip)], &[], None).await;
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
    let back = got.windows(2).filter(|w| w[1].received < w[0].received).count();
    assert_eq!(back, 0, "the packets' instants go back {back} times in {}", got.len());
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

/// A WAV server that writes an endless A-law stream at 8 kHz as fast as the
/// connection takes it, and counts the bytes it got written.
async fn fast_wav_server(written: Arc<std::sync::atomic::AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else { return };
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        let mut head = b"HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n".to_vec();
        head.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00");
        head.extend_from_slice(&[6, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0]);
        head.extend_from_slice(b"data\xff\xff\xff\xff");
        if socket.write_all(&head).await.is_err() {
            return;
        }
        let block = vec![0xd5u8; 64 * 1024];
        while socket.write_all(&block).await.is_ok() {
            written.fetch_add(block.len(), std::sync::atomic::Ordering::Relaxed);
        }
    });
    format!("http://{addr}/camera_1_src")
}

/// A WAV served faster than time (a file sent at the network's speed) is
/// read no faster than the core lets a source run ahead of the wall clock
/// (`MAX_LEAD_SECS`): TCP holds the rest at the server, and the transcode
/// costs what real time does. Read as it came, the reader took 4 s of such a
/// server at whatever rate the transcoder could encode - hundreds of seconds
/// of sound a second, on a runtime thread - all of it published at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_wav_served_faster_than_time_is_read_at_most_the_lead_ahead() {
    let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let url = fast_wav_server(written.clone()).await;
    let h = Harness::start_native(&no_ffmpeg(), vec![], &[DEDUP_INPUT], http()).await;
    let spec = format!("ffmpeg:{url}#audio=pcmu#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let tag = ConsumerTag::new("test").with_class(oxrtc_core::ConsumerClass::Recorder);
    let mut sub = sound.subscribe(&Want::from_query("audio").unwrap(), tag).await.unwrap();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(5);
    let mut packets = 0usize;
    while let Ok(event) = tokio::time::timeout_at(deadline, sub.recv()).await {
        if let Ok(Event::Frame { .. }) = event {
            packets += 1;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let bytes = written.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("{packets} packets of 20 ms in {elapsed:.1} s; the server wrote {bytes} bytes");
    // The lead and the time elapsed (15 s here), and room for the piece the
    // HTTP client had already read when the lead was reached (up to its
    // buffer, a few seconds of 8 kHz sound): 25 s, against 9000 s unpaced.
    let bound = (oxrtc_core::timeline::MAX_LEAD_SECS as f64 + elapsed + 10.0) * 50.0;
    assert!((packets as f64) < bound, "{packets} packets ({:.0} s of sound) in {elapsed:.1} s", packets as f64 / 50.0);
}

/// An HTTP input that is not a WAV is FFmpeg's from the second run on: the
/// native `GET` is made once. Asked again each run, every attempt opened the
/// server twice (the native `GET`, then FFmpeg's), which a camera that
/// rations its connections may refuse every time.
#[tokio::test(flavor = "multi_thread")]
async fn an_http_input_found_not_wav_is_not_asked_again() {
    let Some(bin) = ffmpeg() else { return };
    let (native, lavf) =
        (Arc::new(std::sync::atomic::AtomicUsize::new(0)), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counts = (native.clone(), lavf.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            let who = if head.contains("user-agent: lavf") { &counts.1 } else { &counts.0 };
            who.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = socket.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nnot a sound\n").await;
        }
    });
    let h = Harness::start_native(&bin, vec![], &[DEDUP_INPUT], http()).await;
    let spec = format!("ffmpeg:http://{addr}/camera_1_src#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let _sub = sound.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while lavf.load(std::sync::atomic::Ordering::Relaxed) < 3 {
        assert!(Instant::now() < deadline, "FFmpeg ran {} times", lavf.load(std::sync::atomic::Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (native, lavf) =
        (native.load(std::sync::atomic::Ordering::Relaxed), lavf.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(native, 1, "the native reader asked {native} times for FFmpeg's {lavf}");
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
