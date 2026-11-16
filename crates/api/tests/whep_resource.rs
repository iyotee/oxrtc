//! The WHEP session resource: the `Location` of an answer, `PATCH` for
//! trickled candidates and `DELETE` for the end of the session.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode, header};
use common::*;
use oxrtc_api::{AnswerError, ResourceError};
use oxrtc_core::Codec;
use std::sync::Arc;

const TRICKLE: &[u8] = b"application/trickle-ice-sdpfrag";
const FRAGMENT: &str = "a=ice-ufrag:EsAw\r\na=candidate:1 1 udp 2122260223 192.168.1.20 50000 typ host\r\n";

async fn api_with(answerer: &Arc<FakeAnswerer>) -> Api {
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    api
}

fn resource_path() -> String {
    format!("/api/webrtc/{RESOURCE}")
}

async fn patch(api: &Api, path: &str, content_type: Option<&[u8]>, body: &str) -> Reply {
    let auth = basic(USER, PASS);
    let headers: Vec<(header::HeaderName, &[u8])> =
        content_type.map(|c| (header::CONTENT_TYPE, c)).into_iter().collect();
    api.send_with(Method::PATCH, path, body.as_bytes().to_vec(), Some(&auth), &headers).await
}

#[tokio::test]
async fn the_answer_names_its_session_and_keeps_what_go2rtc_clients_read() {
    let answerer = FakeAnswerer::new(Ok("v=0\r\nanswer".into()));
    let api = api_with(&answerer).await;
    let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0\r\n".to_vec(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "go2rtc's status");
    assert_eq!(reply.content_type(), "application/sdp", "go2rtc's media type");
    assert_eq!(reply.text(), "v=0\r\nanswer", "the answer is the body, as before");
    let location = reply.headers.get(header::LOCATION).expect("a Location").to_str().unwrap().to_owned();
    assert_eq!(location, resource_path());
    assert_eq!(reply.headers.get("accept-patch").unwrap(), "application/trickle-ice-sdpfrag");
    // A client that ignores all this still has what it had.
    assert!(!reply.text().is_empty());
}

#[tokio::test]
async fn a_failed_offer_names_no_session() {
    let answerer = FakeAnswerer::new(Err(AnswerError::NoMatchingCodec));
    let api = api_with(&answerer).await;
    let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0\r\n".to_vec(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.headers.get(header::LOCATION).is_none());
}

#[tokio::test]
async fn a_patch_hands_its_fragment_to_the_session() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    let reply = patch(&api, &resource_path(), Some(TRICKLE), FRAGMENT).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(reply.body.is_empty());
    assert_eq!(*answerer.patched.lock().unwrap(), [(RESOURCE.to_owned(), FRAGMENT.to_owned())]);
    // The media type's parameters and case do not matter.
    let reply = patch(&api, &resource_path(), Some(b"Application/Trickle-ICE-SDPFrag; charset=utf-8"), FRAGMENT).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_patch_of_another_media_type_never_reaches_the_session() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    for content_type in [None, Some(&b"application/sdp"[..]), Some(&b"text/plain"[..])] {
        let reply = patch(&api, &resource_path(), content_type, FRAGMENT).await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{content_type:?}");
    }
    assert!(answerer.patched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_patch_body_that_is_not_text_or_is_too_large_is_refused() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    let auth = basic(USER, PASS);
    let headers = [(header::CONTENT_TYPE, TRICKLE)];
    let reply = api.send_with(Method::PATCH, &resource_path(), vec![0xff, 0xfe], Some(&auth), &headers).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let big = vec![b'a'; oxrtc_api::MAX_BODY_BYTES + 1];
    let reply = api.send_with(Method::PATCH, &resource_path(), big, Some(&auth), &headers).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(answerer.patched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_sessions_refusals_keep_their_statuses() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    for (refusal, status) in [
        (ResourceError::NotFound, StatusCode::NOT_FOUND),
        (ResourceError::Bad("too many candidates".into()), StatusCode::BAD_REQUEST),
        (ResourceError::Unsupported("ice restart is not supported".into()), StatusCode::NOT_IMPLEMENTED),
    ] {
        *answerer.refusal.lock().unwrap() = Some(refusal);
        assert_eq!(patch(&api, &resource_path(), Some(TRICKLE), FRAGMENT).await.status, status);
        assert_eq!(api.call(Method::DELETE, &resource_path()).await.status, status);
    }
}

#[tokio::test]
async fn a_delete_ends_the_session_and_says_so() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    let reply = api.call(Method::DELETE, &resource_path()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(*answerer.closed.lock().unwrap(), [RESOURCE.to_owned()]);
}

#[tokio::test]
async fn an_unknown_session_is_404_and_a_malformed_id_never_reaches_the_egress() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    let other = "/api/webrtc/ffffffffffffffffffffffffffffffff";
    assert_eq!(patch(&api, other, Some(TRICKLE), FRAGMENT).await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.call(Method::DELETE, other).await.status, StatusCode::NOT_FOUND);
    assert_eq!(answerer.closed.lock().unwrap().len(), 1, "the unknown id was asked about, and refused by the egress");
    for bad in ["/api/webrtc/a%2Fb", "/api/webrtc/a.b", &format!("/api/webrtc/{}", "a".repeat(65))] {
        assert_eq!(api.call(Method::DELETE, bad).await.status, StatusCode::NOT_FOUND, "{bad}");
        assert_eq!(patch(&api, bad, Some(TRICKLE), FRAGMENT).await.status, StatusCode::NOT_FOUND, "{bad}");
    }
    assert_eq!(answerer.closed.lock().unwrap().len(), 1, "malformed ids stop at the API");
    assert_eq!(answerer.patched.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_resource_is_authenticated_and_only_patch_and_delete() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    let path = resource_path();
    let reply = api.send(Method::DELETE, &path, Vec::new(), None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = api.send(Method::PATCH, &path, FRAGMENT.into(), None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.call(Method::GET, &path).await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(api.call(Method::POST, &path).await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(answerer.closed.lock().unwrap().is_empty() && answerer.patched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_ice_route_is_not_a_session() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let api = api_with(&answerer).await;
    assert_eq!(api.call(Method::DELETE, "/api/webrtc/ice").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(answerer.closed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_webrtc_the_resource_is_not_implemented() {
    let api = Api::new();
    assert_eq!(api.call(Method::DELETE, &resource_path()).await.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(patch(&api, &resource_path(), Some(TRICKLE), FRAGMENT).await.status, StatusCode::NOT_IMPLEMENTED);
}
