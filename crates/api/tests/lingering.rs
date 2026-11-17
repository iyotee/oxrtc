//! A connection the API closes is closed in two steps (RFC 9112, 9.6): the
//! answer and the end of the server's side first, then what the peer still
//! sends is read and dropped for a bounded time before the socket goes. Without
//! it, an answer sent while the client is still writing its request's body
//! ends in a reset, and on some stacks (Windows) the answer is lost with it.
//! Over real TCP on 127.0.0.1:0.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use oxrtc_api::{ApiState, LINGER_BYTES, ServeLimits};
use oxrtc_core::{CancellationToken, Codec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, timeout};

struct Server {
    addr: SocketAddr,
    cancel: CancellationToken,
}

async fn start(limits: ServeLimits) -> Server {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam1", Script::video(Codec::H264, 0));
    let state = ApiState::new(hub(factory), "v", auth()).with_webrtc(FakeAnswerer::new(Ok("v=0".into())));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(oxrtc_api::serve_with_limits(listener, oxrtc_api::router(state), cancel.clone(), limits));
    Server { addr, cancel }
}

/// A request's head announcing `length` bytes of body, none of them sent.
fn head(method: &str, path: &str, auth: Option<&str>, length: usize) -> String {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(value) = auth {
        head.push_str(&format!("Authorization: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
    head
}

/// Waits until the answer's status line has arrived, without taking it from
/// the socket: what a reset then destroys is still there to destroy.
async fn answer_arrived(socket: &TcpStream) {
    let mut seen = [0u8; 12];
    let deadline = Instant::now() + Duration::from_secs(10);
    while socket.peek(&mut seen).await.unwrap() < seen.len() {
        assert!(Instant::now() < deadline, "no answer within 10 s");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A client that writes its head, sees that the server has answered, then
/// goes on writing its body before it reads: `Some(answer)` when it could
/// read the whole answer, `None` when the connection was reset under it.
async fn late_body(addr: SocketAddr, method: &str, path: &str, auth: Option<&str>) -> Option<String> {
    const BODY: usize = 16 * 1024;
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(head(method, path, auth, BODY).as_bytes()).await.unwrap();
    answer_arrived(&socket).await;
    for _ in 0..BODY / 1024 {
        // A server that closed already answers each of these with a reset.
        if socket.write_all(&[b'x'; 1024]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

#[tokio::test]
async fn an_answer_sent_before_the_body_arrived_reaches_a_client_that_writes_on() {
    let server = start(ServeLimits::default()).await;
    let auth = basic(USER, PASS);
    let session = format!("/api/webrtc/{RESOURCE}");
    for (method, path, auth, status) in [
        // The refusals no route reads a body for.
        ("DELETE", session.as_str(), None, "401"),
        ("DELETE", "/api/webrtc/00000000000000000000000000000000", Some(auth.as_str()), "404"),
        ("POST", "/api/nowhere", Some(auth.as_str()), "404"),
        ("PUT", "/api/streams?name=evil&src=exec:calc.exe", Some(auth.as_str()), "400"),
    ] {
        let answer = late_body(server.addr, method, path, auth).await;
        let start_line = answer.as_deref().and_then(|text| text.lines().next());
        assert_eq!(start_line.map(|line| line.contains(status)), Some(true), "{method} {path}: {answer:?}");
    }
    server.cancel.cancel();
}

/// One request on its own connection, read to the end of the server's side;
/// the socket is returned still open.
async fn answered(addr: SocketAddr, head: &str) -> (TcpStream, String) {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    timeout(Duration::from_secs(10), socket.read_to_end(&mut raw)).await.expect("no end of answer").unwrap();
    (socket, String::from_utf8_lossy(&raw).into_owned())
}

/// With one connection allowed, how long a second client waits for its answer:
/// as long as the first connection still holds its slot.
async fn next_is_served_after(addr: SocketAddr) -> Duration {
    let asked = Instant::now();
    let (_socket, answer) = answered(addr, &head("GET", "/api", Some(&basic(USER, PASS)), 0)).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    asked.elapsed()
}

#[tokio::test]
async fn a_lingering_connection_keeps_its_slot_and_ends_at_the_deadline() {
    let linger = Duration::from_millis(400);
    let server = start(ServeLimits { max_connections: 1, linger, ..ServeLimits::default() }).await;
    // Answered and half-closed by the server; this client never closes.
    let (_open, answer) = answered(server.addr, &head("GET", "/api", Some(&basic(USER, PASS)), 0)).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let waited = next_is_served_after(server.addr).await;
    assert!(waited >= linger / 2, "the lingering connection still counted: served after {waited:?}");
    assert!(waited < Duration::from_secs(5), "and was dropped at its deadline: {waited:?}");
    server.cancel.cancel();
}

#[tokio::test]
async fn a_peer_that_writes_on_is_cut_at_the_byte_bound() {
    let linger = Duration::from_secs(60);
    let server = start(ServeLimits { max_connections: 1, linger, ..ServeLimits::default() }).await;
    let mut flood = TcpStream::connect(server.addr).await.unwrap();
    flood.write_all(head("DELETE", "/api/webrtc/unknown", None, 64 * 1024 * 1024).as_bytes()).await.unwrap();
    answer_arrived(&flood).await;
    let writer = tokio::spawn(async move {
        let mut sent = 0usize;
        // Twice the bound at most: the server stops reading at the bound.
        while sent < 2 * LINGER_BYTES && flood.write_all(&[0u8; 4096]).await.is_ok() {
            sent += 4096;
            tokio::task::yield_now().await;
        }
        (flood, sent)
    });
    // Its slot is free again long before the 60 s deadline: the bytes ended it.
    let waited = next_is_served_after(server.addr).await;
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    let (_flood, sent) = timeout(Duration::from_secs(10), writer).await.expect("the writer is stuck").unwrap();
    assert!(sent >= LINGER_BYTES / 2, "the bound was reached by writing: {sent} bytes");
    server.cancel.cancel();
}

#[tokio::test]
async fn a_stop_ends_the_lingering_connections_at_once() {
    let linger = Duration::from_secs(60);
    let server = start(ServeLimits { linger, ..ServeLimits::default() }).await;
    let (mut open, answer) = answered(server.addr, &head("GET", "/api", Some(&basic(USER, PASS)), 0)).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    // Still lingering: what is written now is taken, not reset. A socket the
    // server had closed takes the first write and refuses the next ones, so
    // several are made, apart.
    for _ in 0..10 {
        open.write_all(b"x").await.expect("the connection was closed before the stop");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    server.cancel.cancel();
    // The socket is gone: the peer's writes start failing well before 60 s.
    let deadline = Instant::now() + Duration::from_secs(5);
    while open.write_all(b"x").await.is_ok() {
        assert!(Instant::now() < deadline, "still open 5 s after the stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
