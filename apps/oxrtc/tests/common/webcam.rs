//! A fake MJPEG webcam over HTTP (`multipart/x-mixed-replace`, behind Basic
//! authentication, and one picture per request), shared by the tests that
//! read the owner's webcams.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

use super::Tools;

pub const CAMERA_USER: &str = "viewer";
pub const CAMERA_PASS: &str = "hunter2-webcam";

/// Fifteen real JPEG pictures (640x360, 4:2:0), as FFmpeg's encoder writes
/// them: with the standard Huffman tables RTP/JPEG carries (`default`) or with
/// tables fitted to each picture (`optimal`, the encoder's own default).
pub async fn pictures(tools: &Tools, dir: &std::path::Path, huffman: &str) -> Vec<Vec<u8>> {
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
pub struct Webcam {
    pub port: u16,
    pub connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Webcam {
    pub async fn start(pictures: Vec<Vec<u8>>) -> Self {
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
