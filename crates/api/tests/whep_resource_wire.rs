//! A refused `PATCH` still answers a client that sends its body after its head,
//! over real TCP: a server that replies and closes before reading the body
//! resets the connection, and on some stacks (Windows) the reply is lost with it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{CancellationToken, Codec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FRAGMENT: &[u8] = b"a=candidate:1 1 udp 2122260223 192.168.1.20 50000 typ host\r\na=end-of-candidates\r\n";

/// One `PATCH` on its own connection (`Connection: close`), its body sent a
/// moment after its head, as a client that writes them apart does.
async fn patch(addr: std::net::SocketAddr, content_type: &str, path: &str, auth: &str) -> Option<String> {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "PATCH {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAuthorization: {auth}\r\n\
         Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        FRAGMENT.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The server may already have answered and closed: this write can fail.
    let _ = socket.write_all(FRAGMENT).await;
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

#[tokio::test]
async fn a_patch_refused_before_its_body_is_read_is_still_answered() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam1", Script::video(Codec::H264, 0));
    let state = ApiState::new(hub(factory.clone()), "v", auth()).with_webrtc(answerer.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let router = oxrtc_api::router(state);
    tokio::spawn(oxrtc_api::serve_with_limits(listener, router, cancel.clone(), ServeLimits::default()));

    let auth = basic(USER, PASS);
    let path = format!("/api/webrtc/{RESOURCE}");
    let wrong_type = patch(addr, "application/sdp", &path, &auth).await;
    assert!(wrong_type.as_deref().is_some_and(|r| r.starts_with("HTTP/1.1 415")), "{wrong_type:?}");
    let unknown = patch(addr, "application/trickle-ice-sdpfrag", "/api/webrtc/not*an*id", &auth).await;
    assert!(unknown.as_deref().is_some_and(|r| r.starts_with("HTTP/1.1 404")), "{unknown:?}");
    cancel.cancel();
}
