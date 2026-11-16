//! The class a transcode's input is read as (D18, R23b), seen from the stream
//! it reads: the RTSP reader an `ffmpeg:` run opens on `camera_N_src` is
//! listed as `queue: live` when the run makes a picture and nothing else, and
//! as `recorder` when it makes sound. Skipped when FFmpeg is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Clip, FixedTarget, Harness, encode_noise, ffmpeg};
use oxrtc_core::{Codec, ConsumerClass, ConsumerSnapshot, ConsumerTag, Event, Want};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// The RTSP readers of `stream` now.
fn readers(h: &Harness, stream: &str) -> Vec<ConsumerSnapshot> {
    let consumers = h.hub.get(stream).unwrap().snapshot().consumers;
    consumers.into_iter().filter(|c| c.kind == "rtsp").collect()
}

/// Waits for the transcode's own reader of `stream` to show.
async fn reader_of(h: &Harness, stream: &str) -> ConsumerSnapshot {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(reader) = readers(h, stream).into_iter().next() {
            return reader;
        }
        assert!(tokio::time::Instant::now() < deadline, "FFmpeg never read {stream}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One RTSP request read from `read`: (method, `CSeq`, `Transport`).
async fn request(read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<(String, String, Option<String>)> {
    let (mut method, mut cseq, mut transport, mut length) = (String::new(), String::new(), None, 0usize);
    let mut line = String::new();
    loop {
        line.clear();
        if read.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let text = line.trim_end();
        if text.is_empty() && !method.is_empty() {
            break;
        }
        if method.is_empty() {
            method = text.split(' ').next().unwrap_or_default().to_owned();
        } else if let Some((name, value)) = text.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "cseq" => cseq = value.trim().to_owned(),
                "transport" => transport = Some(value.trim().to_owned()),
                "content-length" => length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; length];
    read.read_exact(&mut body).await.ok()?;
    Some((method, cseq, transport))
}

/// A publication point that does not answer for `pause`, then takes whatever
/// FFmpeg publishes (ANNOUNCE, SETUP and RECORD answered, the media read and
/// dropped). FFmpeg opens its output after its input and writes nothing until
/// it is answered: it reads none of its input meanwhile, as a process starved
/// of CPU reads little. Returns the URL to publish to.
async fn late_publisher(pause: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("rtsp://{}/late", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else { return };
        tokio::time::sleep(pause).await;
        let (read, mut write) = socket.into_split();
        let mut read = BufReader::new(read);
        while let Some((method, cseq, transport)) = request(&mut read).await {
            let mut answer = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\n");
            if let Some(transport) = transport {
                answer.push_str(&format!("Transport: {transport}\r\n"));
            }
            answer.push_str("\r\n");
            if write.write_all(answer.as_bytes()).await.is_err() {
                return;
            }
            if method == "RECORD" {
                let mut sink = [0u8; 65_536];
                while read.read(&mut sink).await.unwrap_or(0) > 0 {}
                return;
            }
        }
    });
    url
}

/// A transcode that falls behind (here: its output answers 6 s late, so it
/// reads nothing meanwhile; a loaded CPU reads little) is cut to a keyframe
/// when it resumes, after the live budget, 2 s, and so does not trail the
/// picture by the 10 s a recorder is allowed. The pictures are noise, tens of
/// kilobytes each, so that the sockets between the two fill within the pause.
/// The cut is counted when the reader next reads (`dropped`); that it lands
/// on a keyframe is `paused_reader.rs`, on the URL this driver builds.
#[tokio::test(flavor = "multi_thread")]
async fn a_video_transcode_that_falls_behind_is_cut_to_a_keyframe() {
    let Some(bin) = ffmpeg() else { return };
    let source = encode_noise(&bin, "640x360", 3).await;
    let clip = Clip::from_annex_b(Codec::H264, &source);
    let target = Arc::new(FixedTarget(late_publisher(Duration::from_secs(6)).await));
    let h = Harness::start(&bin, vec![("cam", clip)], &[], 4, Duration::from_secs(60), Some(target)).await;
    h.stream("cam_src", &["clip:cam"]);
    let camera = h.stream("cam", &["ffmpeg:cam_src#video=h264#width=320#height=180"]);
    let _viewer = camera.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();

    let first = reader_of(&h, "cam_src").await;
    assert_eq!(first.class, ConsumerClass::Live, "a video transcode shows what it reads: {first:?}");
    // Past the pause FFmpeg reads again, and is told what it lost.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let reader = reader_of(&h, "cam_src").await;
        if reader.dropped > 0 {
            assert_eq!(reader.class, ConsumerClass::Live);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "a 6 s pause was never cut: {reader:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A sound transcode is recorded, so it keeps the recorder's deep queue.
#[tokio::test(flavor = "multi_thread")]
async fn a_sound_transcode_keeps_the_recorder_class() {
    let Some(bin) = ffmpeg() else { return };
    let h = Harness::start(&bin, vec![("mic", Clip::pcma())], &[], 4, Duration::from_secs(20), None).await;
    h.stream("mic_src", &["clip:mic"]);
    let mic = h.stream("mic", &["ffmpeg:mic_src#audio=opus"]);
    let mut sub = mic.subscribe(&Want::from_query("audio").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let mut frames = 0;
    while frames < 10 {
        let event = tokio::time::timeout(Duration::from_secs(30), sub.recv()).await.expect("no sound").unwrap();
        frames += u32::from(matches!(event, Event::Frame { .. }));
    }
    let reader = reader_of(&h, "mic_src").await;
    assert_eq!(reader.class, ConsumerClass::Recorder, "{reader:?}");
}
