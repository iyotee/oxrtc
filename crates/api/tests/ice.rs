//! `PUT /api/webrtc/ice`: new ICE servers for the running egress.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use oxrtc_api::MAX_BODY_BYTES;
use serde_json::json;

async fn put(api: &Api, body: &str) -> Reply {
    api.send(Method::PUT, "/api/webrtc/ice", body.as_bytes().to_vec(), Some(&basic(USER, PASS))).await
}

fn rotation() -> String {
    json!([{
        "urls": ["stun:relay.example:3478", "turn:relay.example:3478?transport=udp", "turn:relay.example:3478?transport=tcp"],
        "username": "1700003600:site",
        "credential": "c2VjcmV0LXRva2Vu",
    }])
    .to_string()
}

#[tokio::test]
async fn the_servers_openivr_writes_reach_the_egress_as_they_are() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    let reply = put(&api, &rotation()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.content_type(), "application/json");
    assert_eq!(reply.json(), json!({"turn": 1, "stun": 1}));
    let seen = answerer.ice.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].len(), 1);
    assert_eq!(seen[0][0].urls.len(), 3);
    assert_eq!(seen[0][0].username.as_deref(), Some("1700003600:site"));
    assert_eq!(seen[0][0].credential.as_deref(), Some("c2VjcmV0LXRva2Vu"));
}

#[tokio::test]
async fn an_empty_list_removes_every_server() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    assert_eq!(put(&api, "[]").await.status, StatusCode::OK);
    assert_eq!(answerer.ice.lock().unwrap().as_slice(), &[Vec::new()]);
}

#[tokio::test]
async fn a_body_that_is_not_the_list_is_refused_without_quoting_it() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    for body in [
        "not json".to_owned(),
        json!({"urls": ["turn:a"], "credential": "hunter2-secret"}).to_string(),
        json!("hunter2-secret").to_string(),
        json!([{"urls": "turn:a", "credential": "hunter2-secret"}]).to_string(),
        json!([{"urls": ["turn:a"], "credential": 12345}]).to_string(),
        String::new(),
    ] {
        let reply = put(&api, &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!reply.text().contains("hunter2"), "the answer quotes the body: {}", reply.text());
    }
    assert!(answerer.ice.lock().unwrap().is_empty(), "nothing reached the egress");
}

#[tokio::test]
async fn what_is_too_large_is_refused() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    let entries: Vec<_> = (0..17).map(|_| json!({"urls": ["stun:a"]})).collect();
    assert_eq!(put(&api, &json!(entries).to_string()).await.status, StatusCode::BAD_REQUEST, "17 entries");
    let urls: Vec<_> = (0..17).map(|i| format!("stun:h{i}")).collect();
    assert_eq!(put(&api, &json!([{"urls": urls}]).to_string()).await.status, StatusCode::BAD_REQUEST, "17 URLs");
    let long = "x".repeat(1025);
    assert_eq!(
        put(&api, &json!([{"urls": ["turn:a"], "credential": long}]).to_string()).await.status,
        StatusCode::BAD_REQUEST
    );
    let fits = "x".repeat(1024);
    assert_eq!(put(&api, &json!([{"urls": ["turn:a"], "credential": fits}]).to_string()).await.status, StatusCode::OK);
    let huge = format!("[{}]", " ".repeat(MAX_BODY_BYTES + 1));
    assert_eq!(put(&api, &huge).await.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(answerer.ice.lock().unwrap().len(), 1, "only the one that fit");
}

#[tokio::test]
async fn without_webrtc_it_is_not_implemented_as_the_offer_route_is() {
    let api = Api::new();
    assert_eq!(put(&api, &rotation()).await.status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn only_put_is_a_method_of_the_route() {
    let api = Api::build(|s| s.with_webrtc(FakeAnswerer::new(Ok("v=0".into()))));
    for method in [Method::GET, Method::POST, Method::DELETE] {
        let reply = api.call(method.clone(), "/api/webrtc/ice").await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
}
