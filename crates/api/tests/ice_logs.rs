//! `PUT /api/webrtc/ice` logs counts, never the credential. Alone in its
//! binary: a log capture races with the callsites of tests that run beside it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

async fn put(api: &Api, body: &str) -> Reply {
    api.send(Method::PUT, "/api/webrtc/ice", body.as_bytes().to_vec(), Some(&basic(USER, PASS))).await
}

fn rotation() -> String {
    json!([{
        "urls": ["stun:relay.example:3478", "turn:relay.example:3478?transport=udp"],
        "username": "1700003600:site",
        "credential": "c2VjcmV0LXRva2Vu",
    }])
    .to_string()
}

#[tokio::test]
async fn no_log_line_carries_the_credential() {
    let (logs, _guard) = Logs::capture();
    let api = Api::build(|s| s.with_webrtc(FakeAnswerer::new(Ok("v=0".into()))));
    assert_eq!(put(&api, &rotation()).await.status, StatusCode::OK);
    let _ = put(&api, r#"[{"urls": 1, "credential": "c2VjcmV0LXRva2Vu"}]"#).await;
    let text = logs.text();
    assert!(text.contains("ice servers replaced"), "{text}");
    assert!(!text.contains("c2VjcmV0LXRva2Vu") && !text.contains("1700003600"), "{text}");
}
