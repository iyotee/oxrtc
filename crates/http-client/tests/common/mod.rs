//! A scripted webcam on 127.0.0.1:0, and a hub with the HTTP factory.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
use oxrtc_core::{ConsumerTag, Drivers, Event, Hub, Limits, SourceState, Stream, Want};
use oxrtc_http_client::{HttpClientConfig, HttpFactory};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// A request the camera received.
#[derive(Debug, Clone)]
pub struct Req {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub at: Instant,
    /// Which connection (counted from 0).
    pub connection: usize,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// What the camera does with a request.
pub enum Reply {
    /// Writes the bytes and keeps the connection for the next request.
    Raw(Vec<u8>),
    /// Writes the bytes and closes.
    Close(Vec<u8>),
    /// Writes the head, then what `next` gives every `every`, until the peer is gone.
    Stream { head: Vec<u8>, next: Box<dyn FnMut() -> Vec<u8> + Send>, every: Duration },
    /// Writes the head, then nothing, until the peer is gone.
    Silent(Vec<u8>),
}

type Handler = Arc<dyn Fn(&Req) -> Reply + Send + Sync>;

pub struct Server {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Req>>>,
    pub connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        Self::serve(listener, port, handler, None).await
    }

    /// The same camera over TLS, with `acceptor`.
    pub async fn start_tls(
        acceptor: tokio_rustls::TlsAcceptor,
        handler: impl Fn(&Req) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        Self::serve(listener, port, handler, Some(acceptor)).await
    }

    async fn serve(
        listener: TcpListener,
        port: u16,
        handler: impl Fn(&Req) -> Reply + Send + Sync + 'static,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let handler: Handler = Arc::new(handler);
        let (log, count) = (requests.clone(), connections.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                let (handler, log) = (handler.clone(), log.clone());
                let connection = count.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            if let Ok(tls) = acceptor.accept(socket).await {
                                talk(tls, handler, log, connection).await;
                            }
                        }
                        None => talk(socket, handler, log, connection).await,
                    }
                });
            }
        });
        Self { port, requests, connections, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

async fn talk<I: AsyncRead + AsyncWrite + Unpin>(
    mut io: I,
    handler: Handler,
    log: Arc<Mutex<Vec<Req>>>,
    connection: usize,
) {
    let mut buf = Vec::new();
    loop {
        let end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 2048];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end);
        let mut lines = head.lines();
        let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_owned();
        let headers =
            lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
        let request = Req { path, headers, at: Instant::now(), connection };
        log.lock().unwrap().push(request.clone());
        match handler(&request) {
            Reply::Raw(bytes) => {
                if io.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            Reply::Close(bytes) => {
                let _ = io.write_all(&bytes).await;
                let _ = io.shutdown().await;
                return;
            }
            Reply::Stream { head, mut next, every } => {
                if io.write_all(&head).await.is_err() {
                    return;
                }
                loop {
                    if io.write_all(&next()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(every).await;
                }
            }
            Reply::Silent(head) => {
                let _ = io.write_all(&head).await;
                std::future::pending::<()>().await;
            }
        }
    }
}

/// A picture the driver takes for a JPEG: it starts with the SOI marker.
pub fn picture(tag: u8, size: usize) -> Vec<u8> {
    let mut data = vec![0xff, 0xd8, tag];
    data.extend((0..size).map(|i| (i % 200) as u8 + 1));
    data.extend_from_slice(&[0xff, 0xd9]);
    data
}

/// `HTTP/1.1 200` with a `Content-Length` body.
pub fn ok(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n", body.len())
        .into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn status(code: u16, headers: &[&str]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {code} X\r\n");
    for h in headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out.into_bytes()
}

/// One part of a `multipart/x-mixed-replace` stream, with its length.
pub fn part(delimiter: &str, picture: &[u8]) -> Vec<u8> {
    let mut out =
        format!("{delimiter}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", picture.len()).into_bytes();
    out.extend_from_slice(picture);
    out.extend_from_slice(b"\r\n");
    out
}

/// A camera streaming `pictures` in turn, as HTTP/1.0 with no length, the way
/// most small ones do.
pub fn stream_of(boundary_param: &str, delimiter: &'static str, pictures: Vec<Vec<u8>>, every: Duration) -> Reply {
    let mut i = 0;
    Reply::Stream {
        head: format!("HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary={boundary_param}\r\n\r\n")
            .into_bytes(),
        next: Box::new(move || {
            i += 1;
            part(delimiter, &pictures[(i - 1) % pictures.len()])
        }),
        every,
    }
}

/// The check a Digest-protected camera does, from RFC 7616 (MD5, qop=auth).
pub fn digest_ok(authorization: Option<&str>, path: &str, user: &str, pass: &str, realm: &str, nonce: &str) -> bool {
    let Some(rest) = authorization.and_then(|a| a.strip_prefix("Digest ")) else { return false };
    let field = |name: &str| -> Option<String> {
        let at = rest.find(&format!("{name}="))?;
        let value = &rest[at + name.len() + 1..];
        match value.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().map(str::to_owned),
            None => value.split(',').next().map(|v| v.trim().to_owned()),
        }
    };
    let md5 = |s: String| format!("{:x}", Md5::digest(s.as_bytes()));
    let (Some(u), Some(r), Some(n), Some(uri), Some(response)) =
        (field("username"), field("realm"), field("nonce"), field("uri"), field("response"))
    else {
        return false;
    };
    if u != user || r != realm || n != nonce || uri != path {
        return false;
    }
    let ha1 = md5(format!("{user}:{realm}:{pass}"));
    let ha2 = md5(format!("GET:{path}"));
    let expected = match (field("qop"), field("nc"), field("cnonce")) {
        (Some(qop), Some(nc), Some(cnonce)) => md5(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}")),
        _ => md5(format!("{ha1}:{nonce}:{ha2}")),
    };
    response == expected
}

/// A hub with the HTTP factory, quick retries and a short idle grace.
pub fn hub_with(config: HttpClientConfig) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(HttpFactory::new(config)));
    Hub::new(
        drivers,
        Limits {
            probe_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(20),
            stall_timeout: Duration::from_secs(20),
            idle_grace: Duration::from_millis(100),
            backoff_initial: Duration::from_millis(200),
            backoff_max: Duration::from_secs(1),
            ..Limits::default()
        },
    )
}

pub fn hub() -> Arc<Hub> {
    hub_with(HttpClientConfig::default())
}

/// The first `count` pictures a viewer of `stream` gets, and the track list.
pub async fn watch(stream: &Arc<Stream>, count: usize) -> (Vec<oxrtc_core::TrackInfo>, Vec<Vec<u8>>) {
    let mut sub = tokio::time::timeout(Duration::from_secs(10), stream.subscribe(&Want::all(), ConsumerTag::new("t")))
        .await
        .unwrap()
        .unwrap();
    let (mut tracks, mut pictures) = (Vec::new(), Vec::new());
    while pictures.len() < count {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.unwrap().unwrap() {
            Event::Tracks(t) => tracks = t.to_vec(),
            Event::Frame { frame, .. } => {
                assert!(frame.keyframe, "a picture is a keyframe");
                pictures.push(frame.data.to_vec());
            }
            Event::Discontinuity => {}
        }
    }
    (tracks, pictures)
}

/// Creates demand on the stream's sources, as a viewer would, and returns
/// the error its first source reports once it is retrying.
pub async fn failure_of(stream: &Arc<Stream>, within: Duration) -> String {
    let stream_for_demand = stream.clone();
    let demand = tokio::spawn(async move {
        let _sub = stream_for_demand.subscribe(&Want::all(), ConsumerTag::new("t")).await;
        std::future::pending::<()>().await;
    });
    let deadline = Instant::now() + within;
    let error = loop {
        match stream.snapshot().sources[0].state.clone() {
            SourceState::Retrying { error, .. } | SourceState::Failed { error } => break error,
            other => assert!(Instant::now() < deadline, "still {other:?} after {within:?}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    demand.abort();
    error
}
