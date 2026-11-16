//! A session resource's id is a capability: no log line carries it, not even
//! the one for a request refused for want of credentials (which names the
//! path). Alone in its binary: a log capture races with the callsites of tests
//! that run beside it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode, header};
use common::*;
use oxrtc_core::Codec;

#[tokio::test]
async fn no_log_line_carries_a_resource_id() {
    let (logs, _guard) = Logs::capture();
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let path = format!("/api/webrtc/{RESOURCE}");

    // Refused for want of credentials: the line that names the path.
    assert_eq!(api.send(Method::DELETE, &path, Vec::new(), None).await.status, StatusCode::UNAUTHORIZED);
    let trickle: &[u8] = b"application/trickle-ice-sdpfrag";
    let fragment = b"a=candidate:1 1 udp 2122260223 192.168.1.20 50000 typ host\r\n".to_vec();
    let denied = api.send_with(Method::PATCH, &path, fragment.clone(), None, &[(header::CONTENT_TYPE, trickle)]).await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    // Accepted ones.
    let auth = basic(USER, PASS);
    let patched = api.send_with(Method::PATCH, &path, fragment, Some(&auth), &[(header::CONTENT_TYPE, trickle)]).await;
    assert_eq!(patched.status, StatusCode::NO_CONTENT);
    assert_eq!(api.send(Method::DELETE, &path, Vec::new(), Some(&auth)).await.status, StatusCode::OK);

    let text = logs.text();
    assert!(text.contains("unauthorized request"), "the refusal is still logged: {text}");
    assert!(!text.contains(RESOURCE), "{text}");

    // The routes that are not sessions keep their real path in the line.
    let (logs, _guard) = Logs::capture();
    assert_eq!(api.send(Method::PUT, "/api/webrtc/ice", b"[]".to_vec(), None).await.status, StatusCode::UNAUTHORIZED);
    assert!(logs.text().contains("/api/webrtc/ice"), "{}", logs.text());
}
