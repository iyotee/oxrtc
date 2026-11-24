//! Motion JPEG on the running server, the real path (F5): go2rtc and FFmpeg
//! as the camera, a fake HTTP webcam, oxrtc in process. FFmpeg's own HTTP
//! client reads `stream.mjpeg` of an H.264 camera (its transcode) and decodes
//! fifty pictures with no error; three readers of it share one FFmpeg, which
//! stops once they are gone; a webcam's pictures are passed on as they came,
//! with no FFmpeg; `frame.jpeg` scales and turns. Skipped (and passed) when
//! FFmpeg or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use common::http::Request;
use common::webcam::{self, Webcam};
use common::*;
use oxrtc::App;
use oxrtc_api::config::parse;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";

/// A reader of `stream.mjpeg` over HTTP/1.0 (a body to the connection's end,
/// as an old client reads it): its pictures, part by part.
struct Reader {
    stream: TcpStream,
    held: Vec<u8>,
}

impl Reader {
    async fn open(api: SocketAddr, src: &str) -> Self {
        let mut stream = TcpStream::connect(api).await.unwrap();
        let credential = base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"));
        let request = format!("GET /api/stream.mjpeg?src={src} HTTP/1.0\r\nAuthorization: Basic {credential}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut me = Self { stream, held: Vec::new() };
        let head_end = me.until(b"\r\n\r\n").await;
        let head = String::from_utf8_lossy(&me.held[..head_end]).to_ascii_lowercase();
        assert!(head.starts_with("http/1.0 200") || head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-type: multipart/x-mixed-replace; boundary=frame"), "{head}");
        me.held.drain(..head_end + 4);
        me
    }

    /// Reads until `needle` is held; where it starts.
    async fn until(&mut self, needle: &[u8]) -> usize {
        loop {
            if let Some(at) = self.held.windows(needle.len()).position(|w| w == needle) {
                return at;
            }
            let mut chunk = [0u8; 65536];
            let n = tokio::time::timeout(Duration::from_secs(30), self.stream.read(&mut chunk)).await.unwrap().unwrap();
            assert!(n > 0, "the stream ended");
            self.held.extend_from_slice(&chunk[..n]);
        }
    }

    /// The next picture, by its part's `Content-Length`.
    async fn picture(&mut self) -> Vec<u8> {
        let head_end = self.until(b"\r\n\r\n").await;
        let head = String::from_utf8_lossy(&self.held[..head_end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap_or_else(|| panic!("no length in {head:?}"))
            .parse()
            .unwrap();
        let start = head_end + 4;
        while self.held.len() < start + length + 2 {
            let mut chunk = [0u8; 65536];
            let n = tokio::time::timeout(Duration::from_secs(30), self.stream.read(&mut chunk)).await.unwrap().unwrap();
            assert!(n > 0, "a part cut short");
            self.held.extend_from_slice(&chunk[..n]);
        }
        let picture = self.held[start..start + length].to_vec();
        self.held.drain(..start + length + 2);
        picture
    }
}

/// The FFmpeg processes oxrtc (in this process) runs: those of the Motion
/// JPEG transcodes when no other runs (the readers here are not FFmpeg). A
/// zombie is not one: on Linux a stopped transcode stays one until reaped.
fn jpeg_encoders() -> Vec<u32> {
    still_running(&ffmpeg_children(std::process::id()))
}

/// What `pids` are, for a failure's message: each one's command line.
fn described(pids: &[u32]) -> String {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    pids.iter()
        .map(|pid| {
            let cmd = system.process(sysinfo::Pid::from_u32(*pid)).map(|p| p.cmd().join(" ")).unwrap_or_default();
            format!("{pid}: {cmd}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn settle(check: impl Fn() -> bool, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check()
}

#[tokio::test]
async fn ffmpeg_reads_stream_mjpeg_one_transcode_serves_every_reader_and_a_webcam_is_passed_on() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start(&tools).await;
    let dir = temp_dir("mjpeg");
    let pictures = webcam::pictures(&tools, &dir, "optimal").await;
    let webcam = Webcam::start(pictures.clone()).await;
    let text = format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{}\"\n\
         streams:\n  cam: \"{}\"\n  hevc: \"{}\"\n  webcam: \"http://{}:{}@127.0.0.1:{}/video.mjpg\"\n",
        tools.ffmpeg.display(),
        camera.url("h264"),
        camera.url("h265"),
        webcam::CAMERA_USER,
        webcam::CAMERA_PASS,
        webcam.port,
    );
    let app = App::start(&parse(&text).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;

    // FFmpeg's HTTP client, as `ffplay http://.../api/stream.mjpeg?src=` reads it.
    for src in ["cam", "hevc"] {
        let url = format!("http://{USER}:{PASS}@{api}/api/stream.mjpeg?src={src}");
        let output = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-v", "error", "-xerror", "-i", &url, "-frames:v", "50", "-f", "framemd5", "-"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        let errors = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        assert!(output.status.success() && errors.is_empty(), "{src}: {errors}");
        let decoded: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(str::to_owned)
            .collect();
        assert_eq!(decoded.len(), 50, "{src}: fifty pictures decoded");
        let sums: std::collections::HashSet<&str> = decoded.iter().filter_map(|l| l.rsplit(',').next()).collect();
        assert!(sums.len() > 10, "{src}: the pictures move ({} different)", sums.len());
    }

    // The transcodes of the readers above stop with them (their stop takes a
    // moment); then three readers at once: one FFmpeg for the stream.
    if !settle(|| jpeg_encoders().is_empty(), Duration::from_secs(10)).await {
        let mut listings = String::new();
        for src in ["cam", "hevc"] {
            let listing = Request::new("GET", &format!("/api/streams?src={src}")).auth(USER, PASS).send(api).await;
            listings.push_str(&format!("\n{src}: {}", String::from_utf8_lossy(&listing.body)));
        }
        panic!("the decoders' transcodes stop; still running:\n{}{listings}", described(&jpeg_encoders()));
    }
    let mut readers = Vec::new();
    for _ in 0..3 {
        readers.push(Reader::open(api, "cam").await);
    }
    for reader in &mut readers {
        let picture = reader.picture().await;
        assert_eq!(jpeg_size(&picture), Some((1280, 720)));
    }
    assert_eq!(jpeg_encoders().len(), 1, "one transcode for every reader of the stream");
    let listing = Request::new("GET", "/api/streams?src=cam").auth(USER, PASS).send(api).await.json();
    let mjpeg = listing["consumers"].as_array().unwrap().iter().filter(|c| c["format_name"] == "mjpeg").count();
    assert_eq!(mjpeg, 3, "the stream lists its transcode's readers: {listing}");
    drop(readers);
    assert!(
        settle(|| jpeg_encoders().is_empty(), Duration::from_secs(10)).await,
        "the transcode stops with its readers"
    );

    // The webcam's own pictures, untouched, and no FFmpeg for them.
    let mut reader = Reader::open(api, "webcam").await;
    for _ in 0..5 {
        let picture = reader.picture().await;
        assert!(pictures.contains(&picture), "a picture of the webcam's own, as it came");
    }
    assert!(jpeg_encoders().is_empty(), "no transcode for a stream that has JPEG");
    drop(reader);

    // `frame.jpeg` scaled then turned, and asked as it is.
    let still = Request::new("GET", "/api/frame.jpeg?src=cam&width=320&rotate=90").auth(USER, PASS).send(api).await;
    assert_eq!(jpeg_size(&still.body), Some((180, 320)));
    let still = Request::new("GET", "/api/frame.jpeg?src=webcam").auth(USER, PASS).send(api).await;
    assert!(pictures.contains(&still.body), "the webcam's picture as it is");
    let still = Request::new("GET", "/api/frame.jpeg?src=webcam&h=180").auth(USER, PASS).send(api).await;
    assert_eq!(jpeg_size(&still.body), Some((320, 180)), "a webcam's picture is scaled too (go2rtc does not)");

    drop(webcam);
    camera.stop().await;
    assert!(app.shutdown().await);
    remove_dir(&dir).await;
}
