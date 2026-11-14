//! The API over real TCP on 127.0.0.1:0: authentication from loopback, the
//! peer address handed to WebRTC, the connection limit and the shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{CancellationToken, Codec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

async fn start(state: ApiState, max_connections: usize, idle_timeout: Duration) -> Server {
    start_with(state, ServeLimits { max_connections, idle_timeout, ..ServeLimits::default() }).await
}

async fn start_with(state: ApiState, limits: ServeLimits) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let router = oxrtc_api::router(state);
    let task = tokio::spawn(oxrtc_api::serve_with_limits(listener, router, cancel.clone(), limits));
    Server { addr, cancel, task }
}

/// Sends one request on `stream` and reads one response (headers and a
/// `Content-Length` body).
async fn exchange(stream: &mut TcpStream, method: &str, path: &str, auth: Option<&str>, body: &[u8]) -> (u16, String) {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n", body.len());
    if let Some(value) = auth {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed before a full response");
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = &text[..end];
            let length = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned()))
                .map_or(0, |v| v.parse::<usize>().unwrap());
            if buf.len() >= end + 4 + length {
                let status = head[9..12].parse().unwrap();
                return (status, text);
            }
        }
    }
}

#[tokio::test]
async fn loopback_is_authenticated_too() {
    let factory = Arc::new(FakeFactory::default());
    let hub = hub(factory.clone());
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let state = ApiState::new(hub.clone(), "v", auth()).with_webrtc(answerer.clone());
    let server = start(state, 16, Duration::from_secs(60)).await;
    for (method, path) in ROUTES {
        let mut stream = TcpStream::connect(server.addr).await.unwrap();
        let (status, text) = exchange(&mut stream, method, path, None, b"").await;
        assert_eq!(status, 401, "{method} {path} from 127.0.0.1");
        assert!(text.to_ascii_lowercase().contains("www-authenticate: basic"), "{text}");
    }
    assert!(hub.is_empty());

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let local = stream.local_addr().unwrap();
    let auth = basic(USER, PASS);
    factory.add("cam1", Script::video(Codec::H264, 0));
    let (status, _) = exchange(&mut stream, "PUT", "/api/streams?name=cam&src=fake:cam1", Some(&auth), b"").await;
    assert_eq!(status, 200);
    let (status, text) = exchange(&mut stream, "POST", "/api/webrtc?src=cam", Some(&auth), b"v=0 offer").await;
    assert_eq!(status, 201, "{text}");
    assert_eq!(answerer.seen.lock().unwrap()[0].2, Some(local), "the answerer learns the viewer's address");

    server.cancel.cancel();
    drop(stream);
    let ended = tokio::time::timeout(Duration::from_secs(5), server.task).await;
    assert!(ended.unwrap().unwrap().is_ok(), "serve returns once cancelled");
}

#[tokio::test]
async fn connection_limit() {
    let state = ApiState::new(hub(Arc::new(FakeFactory::default())), "v", auth());
    let server = start(state, 2, Duration::from_secs(60)).await;
    let auth = basic(USER, PASS);
    let mut first = TcpStream::connect(server.addr).await.unwrap();
    let mut second = TcpStream::connect(server.addr).await.unwrap();
    assert_eq!(exchange(&mut first, "GET", "/api", Some(&auth), b"").await.0, 200);
    assert_eq!(exchange(&mut second, "GET", "/api", Some(&auth), b"").await.0, 200);

    let mut third = TcpStream::connect(server.addr).await.unwrap();
    let waiting =
        tokio::time::timeout(Duration::from_millis(400), exchange(&mut third, "GET", "/api", Some(&auth), b""));
    assert!(waiting.await.is_err(), "a third connection is not served while two are open");

    drop(first);
    let served = tokio::time::timeout(Duration::from_secs(5), async {
        let mut third = third;
        exchange(&mut third, "GET", "/api", Some(&auth), b"").await
    });
    // The request already written is read once the connection is accepted:
    // the second exchange's own request is queued behind it.
    assert_eq!(served.await.unwrap().0, 200, "served once a slot frees");
    server.cancel.cancel();
}

#[tokio::test]
async fn oversized_body_is_refused_up_front() {
    let state = ApiState::new(hub(Arc::new(FakeFactory::default())), "v", auth());
    let server = start(state, 4, Duration::from_secs(60)).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let request = format!(
        "POST /api/webrtc?src=x HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n",
        basic(USER, PASS),
        10 * 1024 * 1024
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&buf[..n]);
    assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    server.cancel.cancel();
}

#[tokio::test]
async fn idle_connections_are_closed() {
    let state = ApiState::new(hub(Arc::new(FakeFactory::default())), "v", auth());
    let server = start(state, 1, Duration::from_millis(300)).await;
    let auth = basic(USER, PASS);

    // A busy connection outlives the idle timeout.
    let mut busy = TcpStream::connect(server.addr).await.unwrap();
    for _ in 0..6 {
        assert_eq!(exchange(&mut busy, "GET", "/api", Some(&auth), b"").await.0, 200);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Silent, it is closed, and its slot goes to the next client.
    let mut buf = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(3), busy.read(&mut buf)).await.unwrap();
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
    let mut next = TcpStream::connect(server.addr).await.unwrap();
    let served = tokio::time::timeout(Duration::from_secs(3), exchange(&mut next, "GET", "/api", Some(&auth), b""));
    assert_eq!(served.await.unwrap().0, 200);

    // A client that opens a connection and never speaks loses it.
    drop(next);
    let mut mute = TcpStream::connect(server.addr).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), mute.read(&mut buf)).await.unwrap();
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
    server.cancel.cancel();
}

#[tokio::test]
async fn a_peer_that_stops_reading_loses_its_connection() {
    let state = ApiState::new(hub(Arc::new(FakeFactory::default())), "v", auth());
    let server = start(state, 1, Duration::from_millis(300)).await;
    let auth = basic(USER, PASS);

    // Pipelined requests, never reading the answers: the server's writes
    // block once the socket buffers are full, and stay blocked.
    let request = format!("GET /api HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\n\r\n");
    let batch = request.repeat(4096).into_bytes();
    let stuck = TcpStream::connect(server.addr).await.unwrap();
    let (_reader, mut writer) = stuck.into_split();
    let flood = tokio::spawn(async move {
        for _ in 0..200 {
            if writer.write_all(&batch).await.is_err() {
                return;
            }
        }
    });

    // The only slot frees once the stalled connection has been idle that long.
    let mut next = TcpStream::connect(server.addr).await.unwrap();
    let served = tokio::time::timeout(Duration::from_secs(10), exchange(&mut next, "GET", "/api", Some(&auth), b""));
    assert_eq!(served.await.expect("the stalled connection still holds the only slot").0, 200);
    flood.abort();
    server.cancel.cancel();
}

#[tokio::test]
async fn shutdown_does_not_wait_for_slow_requests() {
    let factory = Arc::new(FakeFactory::default());
    let hub = hub(factory.clone());
    let answerer = FakeAnswerer::slow(Ok("v=0".into()), Duration::from_secs(50));
    let state = ApiState::new(hub, "v", auth()).with_webrtc(answerer);
    let limits = ServeLimits { shutdown_grace: Duration::from_millis(300), ..ServeLimits::default() };
    let server = start_with(state, limits).await;
    factory.add("cam1", Script::video(Codec::H264, 0));
    let auth = basic(USER, PASS);
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    assert_eq!(exchange(&mut stream, "PUT", "/api/streams?name=cam&src=fake:cam1", Some(&auth), b"").await.0, 200);
    let mut slow = TcpStream::connect(server.addr).await.unwrap();
    let pending =
        tokio::spawn(async move { exchange(&mut slow, "POST", "/api/webrtc?src=cam", Some(&auth), b"v=0").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let start = std::time::Instant::now();
    server.cancel.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(5), server.task).await;
    assert!(ended.expect("serve waited for the 50 s request").unwrap().is_ok());
    assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());
    pending.abort();
}
