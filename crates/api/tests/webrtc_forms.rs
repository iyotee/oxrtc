//! `POST /api/webrtc?src=` in go2rtc's other two forms: a JSON offer
//! answered in JSON (Home Assistant's `go2rtc-client`, `forward_whep_sdp_offer`)
//! and a base64 form answered in base64 (M5 F7, D48).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode, header};
use base64::Engine;
use common::*;
use oxrtc_core::Codec;
use serde_json::json;

const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n";

fn api() -> (Api, std::sync::Arc<FakeAnswerer>) {
    let answerer = FakeAnswerer::new(Ok("v=0\r\nanswer".into()));
    let fake = answerer.clone();
    let api = Api::build(move |s| s.with_webrtc(fake));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.hub.upsert("cam", &["fake:cam1"]).unwrap();
    (api, answerer)
}

#[tokio::test]
async fn a_json_offer_is_answered_in_json() {
    let (api, answerer) = api();
    let auth = basic(USER, PASS);
    let body = serde_json::to_vec(&json!({"type": "offer", "sdp": OFFER})).unwrap();
    let kind: &[u8] = b"application/json; charset=utf-8";
    let reply =
        api.send_with(Method::POST, "/api/webrtc?src=cam", body, Some(&auth), &[(header::CONTENT_TYPE, kind)]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.content_type(), "application/json");
    assert_eq!(reply.json(), json!({"type": "answer", "sdp": "v=0\r\nanswer"}), "go2rtc's answer");
    assert!(reply.headers[header::LOCATION].to_str().unwrap().starts_with("/api/webrtc/"), "the session's resource");
    assert_eq!(answerer.seen.lock().unwrap()[0].1, OFFER, "the SDP inside handed to the session");
    // Not the JSON of an offer.
    for bad in [&b"v=0"[..], b"{\"type\":\"offer\"}", b"[1]"] {
        let reply = api
            .send_with(Method::POST, "/api/webrtc?src=cam", bad.to_vec(), Some(&auth), &[(header::CONTENT_TYPE, kind)])
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(bad));
    }
}

#[tokio::test]
async fn a_form_offer_is_answered_in_base64() {
    let (api, answerer) = api();
    let auth = basic(USER, PASS);
    let data = base64::engine::general_purpose::STANDARD.encode(OFFER);
    let body: String = form_urlencoded::Serializer::new(String::new()).append_pair("data", &data).finish();
    let kind: &[u8] = b"application/x-www-form-urlencoded";
    let reply = api
        .send_with(Method::POST, "/api/webrtc?src=cam", body.into_bytes(), Some(&auth), &[(header::CONTENT_TYPE, kind)])
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.content_type(), "application/x-www-form-urlencoded");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(reply.text()).unwrap(), b"v=0\r\nanswer");
    assert_eq!(answerer.seen.lock().unwrap()[0].1, OFFER);
    let reply = api
        .send_with(
            Method::POST,
            "/api/webrtc?src=cam",
            b"data=%%%".to_vec(),
            Some(&auth),
            &[(header::CONTENT_TYPE, kind)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // The SDP itself is WHEP's, `201` as before.
    let sdp: &[u8] = b"application/sdp";
    let reply = api
        .send_with(Method::POST, "/api/webrtc?src=cam", OFFER.into(), Some(&auth), &[(header::CONTENT_TYPE, sdp)])
        .await;
    assert_eq!((reply.status, reply.content_type()), (StatusCode::CREATED, "application/sdp"));
}
