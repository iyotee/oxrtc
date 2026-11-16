//! The binary on the owner's webcams: Motion JPEG over HTTP, as a stream
//! (`multipart/x-mixed-replace`, behind Basic authentication) and as one
//! picture per request, in openIVR's stream layout. What openIVR does with a
//! webcam goes through it: `camera_N_src?video` read over RTSP (FFmpeg, as
//! the recorder and the analysis read it), the picture listed as
//! `video, recvonly, JPEG`, `frame.jpeg`, and a viewer through the H.264
//! transcode. Skipped (and passed) when FFmpeg, ffprobe or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const CAMERA_USER: &str = "viewer";
const CAMERA_PASS: &str = "hunter2-webcam";

/// Fifteen real JPEG pictures (640x360, 4:2:0), as FFmpeg's encoder writes
/// them: with the standard Huffman tables RTP/JPEG carries (`default`) or with
/// tables fitted to each picture (`optimal`, the encoder's own default).
async fn pictures(tools: &Tools, dir: &std::path::Path, huffman: &str) -> Vec<Vec<u8>> {
    let pattern = dir.join("f%02d.jpg");
    let status = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=15"])
        .args(["-frames:v", "15", "-c:v", "mjpeg", "-huffman", huffman, "-pix_fmt", "yuvj420p", "-q:v", "4"])
        .arg(&pattern)
        .stdin(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    (1..=15).map(|i| std::fs::read(dir.join(format!("f{i:02}.jpg"))).unwrap()).collect()
}

/// A webcam: `/video.mjpg` is the stream (Basic authentication), `/snapshot.jpg`
/// one picture per request on a connection kept open. Counts its connections.
struct Webcam {
    port: u16,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Webcam {
    async fn start(pictures: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let pictures = Arc::new(pictures);
        let wanted = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{CAMERA_USER}:{CAMERA_PASS}"))
        );
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let (pictures, wanted) = (pictures.clone(), wanted.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut next = 0usize;
                    loop {
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            let mut chunk = [0u8; 1024];
                            match socket.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        buf.drain(..end);
                        let path = head.lines().next().unwrap().split(' ').nth(1).unwrap().to_owned();
                        if path == "/video.mjpg" {
                            let authorized =
                                head.lines().any(|l| l.eq_ignore_ascii_case(&format!("authorization: {wanted}")));
                            if !authorized {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"webcam\"\r\nContent-Length: 0\r\n\r\n")
                                    .await;
                                continue;
                            }
                            let _ = socket
                                .write_all(b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary=--myboundary\r\n\r\n")
                                .await;
                            loop {
                                let picture = &pictures[next % pictures.len()];
                                next += 1;
                                let mut part = format!(
                                    "--myboundary\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                    picture.len()
                                )
                                .into_bytes();
                                part.extend_from_slice(picture);
                                part.extend_from_slice(b"\r\n");
                                if socket.write_all(&part).await.is_err() {
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(66)).await;
                            }
                        } else if path.starts_with("/snapshot.jpg") {
                            let picture = &pictures[next % pictures.len()];
                            next += 1;
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                picture.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(picture);
                            if socket.write_all(&out).await.is_err() {
                                return;
                            }
                        } else {
                            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                        }
                    }
                });
            }
        });
        Self { port, connections, task }
    }
}

/// openIVR's sources for one camera, by stream name (the layout of
/// `openivr_layout.rs`, without the sound sources: a webcam has none).
fn camera_streams(name: &str, camera_url: &str, rtsp: u16) -> String {
    let src = format!("{name}_src");
    let video = format!("rtsp://127.0.0.1:{rtsp}/{src}?video#timeout=20");
    format!(
        "\x20 {src}: [\"{camera_url}\"]\n\
         \x20 {name}_rec: [\"{video}\", \"ffmpeg:rtsp://127.0.0.1:{rtsp}/{src}#audio=opus#timeout=20\"]\n\
         \x20 {name}: [\"{video}\", \"ffmpeg:{src}#video=h264#width=1920#height=1080#timeout=20\", \
         \"rtsp://127.0.0.1:{rtsp}/{name}_rec?audio=opus#timeout=20\"]\n"
    )
}

async fn wait_healthy(api: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(api).await.is_ok()
            && Request::new("GET", "/api").auth(USER, PASS).send(api).await.status == 200
        {
            return;
        }
        assert!(Instant::now() < deadline, "GET /api did not answer 200 within 10 s of launch");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What the recorder does with a camera whose picture it cannot copy: reads
/// it over RTSP and encodes H.264 (`segment_command(reencode=True)`). The
/// number of pictures out of five seconds of it.
async fn reencoded_frames(tools: &Tools, url: &str) -> u64 {
    let output = Command::new(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-fflags",
            "+genpts+discardcorrupt",
        ])
        .args(["-i", url, "-t", "4", "-map", "0:v:0", "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency"])
        .args(["-pix_fmt", "yuv420p", "-fps_mode", "passthrough", "-an", "-f", "null", "-progress", "pipe:1", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output =
        tokio::time::timeout(Duration::from_secs(60), output).await.expect("the re-encode took over 60 s").unwrap();
    assert!(output.status.success(), "ffmpeg failed on {url}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("frame="))
        .filter_map(|v| v.trim().parse().ok())
        .next_back()
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owners_webcams_are_served_as_a_go2rtc_would_serve_them() {
    let Some(tools) = Tools::find() else { return };
    let dir = temp_dir("webcam");
    let frames = pictures(&tools, &dir, "default").await;
    let webcam = Webcam::start(frames.clone()).await;
    let (api_port, rtsp_port) = (free_tcp_port(), free_tcp_port());
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api_port}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp_port}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n{stream}{snapshot}",
        webrtc = free_udp_port(),
        ffmpeg = tools.ffmpeg.display(),
        // The stream with credentials in the URL, the way openIVR stores a webcam's.
        stream = camera_streams(
            "camera_6",
            &format!("http://{CAMERA_USER}:{CAMERA_PASS}@127.0.0.1:{}/video.mjpg", webcam.port),
            rtsp_port
        ),
        snapshot = camera_streams("camera_7", &format!("http://127.0.0.1:{}/snapshot.jpg", webcam.port), rtsp_port),
    );
    let config = dir.join("oxrtc.yaml");
    std::fs::write(&config, text).unwrap();
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;
    let rtsp = |stream: &str| format!("rtsp://127.0.0.1:{rtsp_port}/{stream}");

    for camera in ["camera_6", "camera_7"] {
        // What the analysis and the recorder read: the webcam's picture, over RTP/JPEG.
        let url = rtsp(&format!("{camera}_src?video"));
        assert_eq!(codecs(&tools, &url).await, "mjpeg", "{camera}");
        assert_eq!(probe(&tools, &url).await, "mjpeg,640,360", "{camera}");

        // The recorder re-encodes it: a recording is H.264.
        let encoded = reencoded_frames(&tools, &rtsp(&format!("{camera}_rec"))).await;
        assert!(encoded >= 20, "{camera}: {encoded} pictures encoded from the recorder's stream");

        // `GET /api/streams`, which openIVR's `_has_sound` reads: a picture, no sound.
        let reader = tokio::spawn({
            let (tools_ffmpeg, url) = (tools.ffmpeg.clone(), url.clone());
            async move {
                Command::new(tools_ffmpeg)
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-rtsp_transport",
                        "tcp",
                        "-i",
                        &url,
                        "-t",
                        "6",
                        "-c",
                        "copy",
                        "-f",
                        "null",
                        "-",
                    ])
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .status()
                    .await
            }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        let listing = Request::new("GET", &format!("/api/streams?src={camera}_src")).auth(USER, PASS).send(api).await;
        assert_eq!(listing.status, 200, "{}", listing.text());
        let json = listing.json();
        assert_eq!(json["producers"][0]["medias"], serde_json::json!(["video, recvonly, JPEG"]), "{camera}: {json}");
        let consumer = &json["consumers"][0];
        assert_eq!(consumer["format_name"], "rtsp", "{camera}: {json}");
        reader.await.unwrap().unwrap();

        // A still of it, as it is.
        let still = Request::new("GET", &format!("/api/frame.jpeg?src={camera}_src")).auth(USER, PASS).send(api).await;
        assert_eq!(still.status, 200, "{}", still.text());
        assert_eq!(still.header("content-type"), Some("image/jpeg"));
        assert!(frames.contains(&still.body), "{camera}: a picture the webcam never sent");
        assert_eq!(jpeg_size(&still.body), Some((640, 360)));

        // A viewer: H.264, through the transcode of the stream's second source.
        let viewer = whep::play(api, (USER, PASS), camera, false, 20, Duration::from_secs(40)).await;
        assert_eq!(viewer.status, 201, "{camera}: {viewer:?}");
        assert!(viewer.video_frames >= 20, "{camera}: {viewer:?}");
        assert_eq!(viewer.codecs, [str0m::format::Codec::H264], "{camera}: {viewer:?}");
    }

    // A webcam has no sound: the recorder's hold of it is refused at once, then taken on the video.
    let held = [
        Request::new("PUT", "/api/preload?src=camera_6_rec&audio=opus").auth(USER, PASS).send(api).await.status,
        Request::new("PUT", "/api/preload?src=camera_6_rec&video=").auth(USER, PASS).send(api).await.status,
    ];
    assert_eq!(held, [500, 200]);

    // The snapshot webcam was asked on one connection, not one per picture.
    assert!(
        webcam.connections.load(Ordering::SeqCst) < 40,
        "{} connections",
        webcam.connections.load(Ordering::SeqCst)
    );

    // Nothing the operator reads names the webcam's password.
    let config = Request::new("GET", "/api/streams").auth(USER, PASS).send(api).await.text();
    assert!(!config.contains(CAMERA_PASS), "the listing shows the password: {config}");
    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let log = oxrtc.drain().await;
    for line in &log {
        assert!(!line.contains(CAMERA_PASS) && !line.contains(PASS), "a password in the log: {line}");
    }
    remove_dir(&dir).await;
}

/// The MD5 of every picture FFmpeg decodes from `input`, in order.
async fn picture_md5s(tools: &Tools, input: &[&str], frames: &str) -> Vec<String> {
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(input)
        .args(["-frames:v", frames, "-f", "framemd5", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(60), output).await.expect("ffmpeg took over 60 s").unwrap();
    assert!(output.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.rsplit(',').next().map(|md5| md5.trim().to_owned()))
        .collect()
}

/// A webcam whose encoder fits the Huffman tables to each picture (FFmpeg's
/// default, libjpeg's `optimize_coding`): RTP/JPEG cannot carry them, so the
/// server codes every scan again with the standard ones. What FFmpeg decodes
/// from the RTSP read is, picture for picture, what it decodes from the
/// camera's own files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_webcam_whose_pictures_have_their_own_huffman_tables_is_read_all_the_same() {
    let Some(tools) = Tools::find() else { return };
    let dir = temp_dir("webcam-optimal");
    let frames = pictures(&tools, &dir, "optimal").await;
    // The luminance AC table of ITU-T T.81 K.5 starts with these counts: none of the pictures has it.
    let standard = [0u8, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
    assert!(frames.iter().all(|f| !f.windows(16).any(|w| w == standard)), "the pictures have the standard tables");
    let webcam = Webcam::start(frames.clone()).await;
    let (api_port, rtsp_port) = (free_tcp_port(), free_tcp_port());
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api_port}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp_port}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n{snapshot}",
        webrtc = free_udp_port(),
        ffmpeg = tools.ffmpeg.display(),
        snapshot = camera_streams("camera_8", &format!("http://127.0.0.1:{}/snapshot.jpg", webcam.port), rtsp_port),
    );
    let config = dir.join("oxrtc.yaml");
    std::fs::write(&config, text).unwrap();
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;
    let url = format!("rtsp://127.0.0.1:{rtsp_port}/camera_8_src?video");

    assert_eq!(probe(&tools, &url).await, "mjpeg,640,360");
    let mut wanted = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let file = dir.join(format!("f{:02}.jpg", index + 1));
        assert_eq!(&std::fs::read(&file).unwrap(), frame);
        wanted.extend(picture_md5s(&tools, &["-i", file.to_str().unwrap()], "1").await);
    }
    let read = picture_md5s(&tools, &["-rtsp_transport", "tcp", "-i", &url], "6").await;
    assert_eq!(read.len(), 6);
    for md5 in &read {
        assert!(wanted.contains(md5), "a picture the webcam never sent: {md5}");
    }
    // The recorder's re-encode gets the pictures too.
    let encoded = reencoded_frames(&tools, &format!("rtsp://127.0.0.1:{rtsp_port}/camera_8_rec")).await;
    assert!(encoded >= 20, "{encoded} pictures encoded from the recorder's stream");
    // Nothing was refused: no warning about a picture the format cannot carry.
    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let log = oxrtc.drain().await;
    assert!(!log.iter().any(|l| l.contains("is not sent")), "{log:?}");
    remove_dir(&dir).await;
}
