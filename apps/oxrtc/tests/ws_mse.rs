//! `mse` on `/api/ws` on the running server, the real path (F3): go2rtc as
//! the camera, oxrtc in process, a client of go2rtc's WebSocket API that asks
//! for MSE with the codec list of go2rtc's player, and FFmpeg reading back
//! what that client received (the init segment and the fragments, one after
//! the other, as a browser's `SourceBuffer` takes them): the codecs, a packet
//! per fragment, decode times that rise, a decode with no error. On openIVR's
//! layout a browser that lists H.265 gets the camera's own with no FFmpeg
//! started for it, one that does not gets the H.264 transcode; a G.711
//! camera's sound comes as FLAC. Skipped (and passed) when FFmpeg or go2rtc
//! is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";
/// What go2rtc's player sends from headless Edge 157 on this PC.
const EDGE: &str = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
/// A browser without H.265.
const NO_HEVC: &str = "avc1.640029,avc1.64002A,avc1.640033,mp4a.40.2,mp4a.40.5,flac,opus";

type Socket = WebSocketStream<TcpStream>;

/// What one MSE reader received.
struct Received {
    mime: String,
    /// The init segment, then every fragment, as one file.
    bytes: Vec<u8>,
    fragments: usize,
}

/// Opens `/api/ws?src=`, asks for MSE with `list`, and keeps what comes for
/// `read` after the first fragment.
async fn read_mse(api: SocketAddr, src: &str, list: &str, read: Duration) -> Received {
    let stream = TcpStream::connect(api).await.unwrap();
    let mut request = format!("ws://{api}/api/ws?src={src}").into_client_request().unwrap();
    let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{USER}:{PASS}"));
    request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Basic {basic}")).unwrap());
    let mut socket: Socket = tokio_tungstenite::client_async(request, stream).await.unwrap().0;
    socket.send(Message::Text(json!({"type": "mse", "value": list}).to_string().into())).await.unwrap();
    let mut received = Received { mime: String::new(), bytes: Vec::new(), fragments: 0 };
    let mut until = tokio::time::Instant::now() + Duration::from_secs(40);
    let mut first = true;
    loop {
        let Ok(item) = tokio::time::timeout_at(until, socket.next()).await else { break };
        match item.expect("the socket is open").unwrap() {
            Message::Text(text) => {
                let message: Value = serde_json::from_str(text.as_str()).unwrap();
                assert_eq!(message["type"], "mse", "{message}");
                assert!(received.mime.is_empty(), "one MIME type: {message}");
                received.mime = message["value"].as_str().unwrap().to_owned();
            }
            Message::Binary(data) => {
                assert!(!received.mime.is_empty(), "the MIME type comes first");
                if !received.bytes.is_empty() {
                    received.fragments += 1;
                    if first {
                        first = false;
                        until = tokio::time::Instant::now() + read;
                    }
                }
                received.bytes.extend_from_slice(&data);
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(received.fragments > 0, "no fragment within 40 s (MIME type {:?})", received.mime);
    received
}

/// ffprobe's codecs, packets per stream and whether each stream's decode
/// times rise, from the file a reader received.
async fn probe_file(tools: &Tools, file: &Path) -> (Vec<String>, Vec<usize>, bool) {
    let output = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "stream=index,codec_name:packet=stream_index,dts", "-of", "json"])
        .arg(file)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "ffprobe: {}", String::from_utf8_lossy(&output.stderr));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let streams = json["streams"].as_array().unwrap();
    let codecs: Vec<String> = streams.iter().map(|s| s["codec_name"].as_str().unwrap().to_owned()).collect();
    let mut counts = vec![0; streams.len()];
    let mut last: Vec<Option<i64>> = vec![None; streams.len()];
    let mut rising = true;
    for packet in json["packets"].as_array().unwrap() {
        let index = packet["stream_index"].as_u64().unwrap() as usize;
        counts[index] += 1;
        let dts = packet["dts"].as_i64().unwrap();
        rising &= last[index].is_none_or(|before| dts > before);
        last[index] = Some(dts);
    }
    (codecs, counts, rising)
}

/// `ffmpeg -xerror` decodes the whole file: its error output, empty when none.
async fn decode_errors(tools: &Tools, file: &Path) -> String {
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-v", "error", "-xerror", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if output.status.success() { errors } else { format!("exit {:?}: {errors}", output.status.code()) }
}

/// Checks what a reader received with FFmpeg; returns ffprobe's counts.
async fn check(tools: &Tools, dir: &Path, name: &str, received: &Received, codecs: &[&str]) -> Vec<usize> {
    let file = dir.join(format!("{name}.mp4"));
    std::fs::write(&file, &received.bytes).unwrap();
    let (read, counts, rising) = probe_file(tools, &file).await;
    assert_eq!(read, codecs, "{name}: ffprobe's codecs ({})", received.mime);
    assert_eq!(counts.iter().sum::<usize>(), received.fragments, "{name}: one packet per fragment: {counts:?}");
    assert!(rising, "{name}: decode times rise on every stream");
    assert_eq!(decode_errors(tools, &file).await, "", "{name}: FFmpeg decodes it with no error");
    counts
}

fn layout(camera: &FakeCamera, rtsp: u16) -> String {
    // openIVR's `camera_N` (go2rtc_manager.py): the camera relayed over the
    // loopback, then its H.264 transcode.
    format!(
        "\x20 camera_1_src: [\"{}#timeout=10\"]\n\
         \x20 camera_1: [\"rtsp://127.0.0.1:{rtsp}/camera_1_src?video#timeout=20\", \
         \"ffmpeg:camera_1_src#video=h264#width=1280#height=720#timeout=20\"]\n\
         \x20 sound: \"{}\"\n",
        camera.url("h265"),
        camera.url("with_sound"),
    )
}

#[tokio::test]
async fn an_mse_reader_receives_what_ffmpeg_and_a_browser_decode() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let rtsp = free_tcp_port();
    let text = format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\nwebrtc:\n  listen: \"{ANY}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{}\"\n\
         streams:\n{}",
        tools.ffmpeg.display(),
        layout(&camera, rtsp)
    );
    let app = App::start(&parse(&text).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    let dir = temp_dir("mse");
    let me = std::process::id();
    // The publisher of `/with_sound` is an FFmpeg of this process too.
    let before = ffmpeg_children(me);

    // A browser that lists H.265: the camera's own, no FFmpeg for it.
    let hevc = read_mse(api, "camera_1", EDGE, Duration::from_secs(3)).await;
    assert!(hevc.mime.starts_with("video/mp4; codecs=\"hvc1."), "{}", hevc.mime);
    let started: Vec<u32> = ffmpeg_children(me).into_iter().filter(|p| !before.contains(p)).collect();
    assert!(started.is_empty(), "no transcode for a browser that takes H.265: {started:?}");
    let counts = check(&tools, &dir, "hevc", &hevc, &["hevc"]).await;
    assert!(counts[0] >= 50, "3 s of 25 pictures a second: {counts:?}");

    // One that does not: the transcode, H.264.
    let avc = read_mse(api, "camera_1", NO_HEVC, Duration::from_secs(3)).await;
    assert!(avc.mime.starts_with("video/mp4; codecs=\"avc1."), "{}", avc.mime);
    check(&tools, &dir, "avc", &avc, &["h264"]).await;

    // A camera's G.711 sound as FLAC, beside its picture.
    let sound = read_mse(api, "sound", EDGE, Duration::from_secs(4)).await;
    assert!(sound.mime.starts_with("video/mp4; codecs=\"avc1.") && sound.mime.ends_with(",flac\""), "{}", sound.mime);
    let counts = check(&tools, &dir, "sound", &sound, &["h264", "flac"]).await;
    // FFmpeg's RTP packs 1024 samples of the 16 kHz A-law a packet: 15.6 a second.
    assert!(counts[0] >= 75 && counts[1] >= 50, "4 s of picture and sound: {counts:?}");

    remove_dir(&dir).await;
    camera.stop().await;
    assert!(app.shutdown().await);
}
