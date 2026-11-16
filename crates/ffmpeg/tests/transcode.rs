//! `ffmpeg:` sources with real FFmpeg, publishing into a real RTSP server:
//! a video transcode of a stream by name, an audio transcode of an RTSP URL,
//! and the failure paths (exit, start timeout, process cap). Skipped when
//! FFmpeg is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Clip, FixedTarget, Harness, encode, ffmpeg, ffprobe, launcher, retry_reason, run};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaKind, Want};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Counts frames of `stream` read with `want` until `wanted` arrive; returns
/// (codec, frames, whether the first frame was a keyframe).
async fn read(stream: &Arc<oxrtc_core::Stream>, want: &str, wanted: u32) -> (Codec, u32, bool) {
    let mut sub = stream.subscribe(&Want::from_query(want).unwrap(), ConsumerTag::new("test")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (mut codec, mut frames, mut first_key) = (None, 0u32, None);
    while frames < wanted {
        let event = tokio::time::timeout_at(deadline, sub.recv()).await.expect("too few frames").unwrap();
        match event {
            Event::Tracks(tracks) => codec = Some(tracks[0].codec),
            Event::Frame { frame, .. } => {
                first_key.get_or_insert(frame.keyframe);
                frames += 1;
            }
            Event::Discontinuity => {}
        }
    }
    (codec.unwrap(), frames, first_key.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_by_name_is_transcoded_to_h264_and_scaled() {
    let Some(bin) = ffmpeg() else { return };
    let source = encode(&bin, Codec::H265, "960x540", 4).await;
    let clip = Clip::from_annex_b(Codec::H265, &source);
    let h = Harness::start(&bin, vec![("camera", clip)], &[], 4, Duration::from_secs(20), None).await;
    h.stream("camera_src", &["clip:camera"]);
    let camera = h.stream("camera", &["ffmpeg:camera_src#video=h264#width=640#height=360"]);
    assert_eq!(camera.snapshot().sources.len(), 1);
    let (codec, frames, first_key) = read(&camera, "video", 50).await;
    assert_eq!(codec, Codec::H264);
    assert!(first_key, "the first frame is a keyframe");
    assert_eq!(frames, 50);
    // The scale took: what the RTSP server serves is 640x360 H.264.
    if let Some(probe) = ffprobe() {
        let url = format!("rtsp://{}/camera?video", h.addr);
        let args = [
            "-v",
            "error",
            "-rtsp_transport",
            "tcp",
            "-show_entries",
            "stream=codec_name,width,height",
            "-of",
            "csv=p=0",
            &url,
        ];
        let (ok, out, err) = run(&probe, &args, Duration::from_secs(30)).await;
        assert!(ok, "ffprobe failed: {err}");
        assert_eq!(out.trim(), "h264,640,360", "stderr: {err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pcma_from_an_rtsp_url_is_transcoded_to_opus() {
    let Some(bin) = ffmpeg() else { return };
    let h = Harness::start(&bin, vec![("mic", Clip::pcma())], &[], 4, Duration::from_secs(20), None).await;
    h.stream("mic_src", &["clip:mic"]);
    let spec = format!("ffmpeg:rtsp://{}/mic_src#audio=opus#timeout=20", h.addr);
    let mic = h.stream("mic", &[spec.as_str()]);
    let (codec, frames, _) = read(&mic, "audio", 50).await;
    assert_eq!(codec, Codec::Opus);
    assert_eq!(frames, 50);
    let tracks = mic.snapshot().sources[0].tracks.clone();
    assert_eq!((tracks[0].kind, tracks[0].clock_rate), (MediaKind::Audio, 48_000));
}

/// What openIVR writes under `ffmpeg:` for its de-duplicated sound
/// (`DEDUP_INPUT`, `go2rtc_manager.py`).
const DEDUP_INPUT: (&str, &str) = ("dedup_input", "-fflags nobuffer -flags low_delay -probesize 32 -i {input}");

/// openIVR's sound de-duplicator (`audio_dedup.py`) as go2rtc sees it: an
/// HTTP/1.0 server that answers `GET /camera_N_src` with an endless WAV of
/// G.711 at 16 kHz (the header's sizes at their maximum), or with `415` for
/// a camera whose sound is not G.711. Returns its address.
async fn dedup_server(status: &'static str) -> std::net::SocketAddr {
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
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &out).await;
                    return;
                }
                out.extend_from_slice(b"Content-Type: audio/wav\r\nConnection: close\r\n\r\n");
                // RIFF, WAVE, fmt (A-law, mono, 16000 Hz, 8 bits), data.
                out.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00");
                out.extend_from_slice(&[6, 0, 1, 0, 0x80, 0x3e, 0, 0, 0x80, 0x3e, 0, 0, 1, 0, 8, 0]);
                out.extend_from_slice(b"data\xff\xff\xff\xff");
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &out).await;
                // 20 ms of silence (A-law 0xd5) a packet, at the pace of a camera.
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if tokio::io::AsyncWriteExt::write_all(&mut socket, &[0xd5u8; 320]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn the_deduplicated_sound_served_over_http_is_transcoded_with_openivrs_input_template() {
    let Some(bin) = ffmpeg() else { return };
    let server = dedup_server("200 OK").await;
    let h = Harness::start(&bin, vec![], &[DEDUP_INPUT], 4, Duration::from_secs(20), None).await;
    let spec = format!("ffmpeg:http://{server}/camera_1_src#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let (codec, frames, _) = read(&sound, "audio=opus", 50).await;
    assert_eq!((codec, frames), (Codec::Opus, 50));
    let tracks = sound.snapshot().sources[0].tracks.clone();
    assert_eq!((tracks[0].kind, tracks[0].codec), (MediaKind::Audio, Codec::Opus));
}

/// A camera whose sound is not G.711 gets a `415` there, and the next source
/// of `camera_N_rec` (the plain transcode) takes over: the run fails, which
/// is what lets the stream fall through (`crates/core/tests/fallback.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn a_deduplicator_that_answers_415_fails_the_run() {
    let Some(bin) = ffmpeg() else { return };
    let server = dedup_server("415 Unsupported Media Type").await;
    let h = Harness::start(&bin, vec![], &[DEDUP_INPUT], 4, Duration::from_secs(20), None).await;
    let spec = format!("ffmpeg:http://{server}/camera_1_src#audio=opus#input=dedup_input");
    let sound = h.stream("camera_1_rec", &[spec.as_str()]);
    let _sub = sound.subscribe(&Want::from_query("audio=opus").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let why = retry_reason(&sound, Duration::from_secs(20)).await;
    assert!(why.starts_with("unreachable: ffmpeg exited"), "{why}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exiting_ffmpeg_fails_the_run_with_its_redacted_error() {
    let Some(bin) = ffmpeg() else { return };
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let h = Harness::start(&bin, vec![], &[], 4, Duration::from_secs(20), None).await;
    let spec = format!("ffmpeg:rtsp://admin:hunter2@{closed}/live#video=h264#timeout=2");
    let stream = h.stream("broken", &[spec.as_str()]);
    let _sub = stream.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let why = retry_reason(&stream, Duration::from_secs(20)).await;
    assert!(why.starts_with("unreachable: ffmpeg exited"), "{why}");
    assert!(!why.contains("hunter2"), "{why}");
    assert!(why.contains("rtsp://***@"), "the error names its input, redacted: {why}");
}

/// A listener that accepts publishers and never answers them.
async fn silent() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("rtsp://{}/silent", listener.local_addr().unwrap());
    (listener, url)
}

/// An input template that ignores `{input}`: a generated picture.
const PATTERN: (&str, &str) = ("pattern", "-re -f lavfi -i testsrc2=size=160x90:rate=5");

/// Reads the accepted connection until it closes; `true` if it did in time.
async fn closes_within(listener: &TcpListener, wait: Duration) -> bool {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await.unwrap().unwrap();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, socket.read(&mut buf)).await {
            Err(_) => return false,
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcoder_that_never_publishes_is_killed_at_the_start_timeout() {
    let Some(bin) = ffmpeg() else { return };
    killed_at_the_start_timeout(&bin).await;
}

/// The same through a launcher (Chocolatey's shim): the FFmpeg it started
/// must die too, or its connection stays open.
#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_and_its_ffmpeg_are_killed_at_the_start_timeout() {
    let Some(bin) = launcher() else { return };
    killed_at_the_start_timeout(&bin).await;
}

async fn killed_at_the_start_timeout(bin: &std::path::Path) {
    let (listener, url) = silent().await;
    let target = Arc::new(FixedTarget(url));
    let h = Harness::start(bin, vec![], &[PATTERN], 4, Duration::from_millis(1500), Some(target)).await;
    let stream = h.stream("never", &["ffmpeg:anything#video=h264#input=pattern"]);
    let _sub = stream.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    // FFmpeg connects and waits for an answer; the run gives up and kills it.
    assert!(closes_within(&listener, Duration::from_secs(8)).await, "ffmpeg was not killed");
    let why = retry_reason(&stream, Duration::from_secs(10)).await;
    assert!(why.contains("no publication within 1.5s"), "{why}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_is_killed_when_the_last_consumer_leaves() {
    let Some(bin) = ffmpeg() else { return };
    let (listener, url) = silent().await;
    let target = Arc::new(FixedTarget(url));
    // A start timeout far beyond the test: only the core's stop can end it.
    let h = Harness::start(&bin, vec![], &[PATTERN], 4, Duration::from_secs(120), Some(target)).await;
    let stream = h.stream("leaving", &["ffmpeg:anything#video=h264#input=pattern"]);
    let sub = stream.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await.unwrap().unwrap();
    // The core drops the run after its idle grace (300 ms here): FFmpeg dies
    // with its handle, and its connection closes.
    drop(sub);
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, socket.read(&mut buf)).await {
            Err(_) => panic!("ffmpeg outlived the run"),
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(_)) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_process_cap_refuses_a_second_transcoder() {
    let Some(bin) = ffmpeg() else { return };
    let (listener, url) = silent().await;
    let target = Arc::new(FixedTarget(url));
    let h = Harness::start(&bin, vec![], &[PATTERN], 1, Duration::from_secs(20), Some(target)).await;
    let first = h.stream("first", &["ffmpeg:anything#video=h264#input=pattern"]);
    let second = h.stream("second", &["ffmpeg:anything#video=h264#input=pattern"]);
    let want = Want::from_query("video").unwrap();
    let _a = first.subscribe(&want, ConsumerTag::new("test")).await.unwrap();
    // Once the first process runs (it has connected), the second is refused.
    let (_socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await.unwrap().unwrap();
    let _b = second.subscribe(&want, ConsumerTag::new("test")).await.unwrap();
    let why = retry_reason(&second, Duration::from_secs(10)).await;
    assert!(why.contains("max_processes"), "{why}");
}
