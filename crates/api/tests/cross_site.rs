//! A page of another site sending the browser's credential (cross-site request
//! forgery, F7 review): a form posted to `/api/restart`, `/api/exit` or
//! `/api/config` is a "simple" request a browser sends with the Basic
//! credential it holds for the server, and the page needs no answer. Every
//! request that changes the server is refused `403` when its `Origin` is not
//! one `api.origin` lets open `/api/ws`; a `GET` from another site never sets a
//! stream from its `src` (go2rtc's `GetOrPatch`). A request with no `Origin`
//! (a program) is taken as before.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode, header};
use common::*;
use oxrtc_api::config::Permissions;
use oxrtc_api::ws::OriginPolicy;
use oxrtc_core::Codec;

const HOST: &[u8] = b"127.0.0.1:1984";

fn allow_all() -> Permissions {
    Permissions { stream_changes: true, config_read: true, config_write: true, restart: true, exit: true }
}

async fn from(api: &Api, method: Method, uri: &str, origin: Option<&[u8]>, site: Option<&[u8]>) -> Reply {
    let mut headers: Vec<(header::HeaderName, &[u8])> = vec![(header::HOST, HOST)];
    if let Some(origin) = origin {
        headers.push((header::ORIGIN, origin));
    }
    if let Some(site) = site {
        headers.push((header::HeaderName::from_static("sec-fetch-site"), site));
    }
    api.send_with(method, uri, b"api: {}\n".to_vec(), Some(&basic(USER, PASS)), &headers).await
}

#[tokio::test]
async fn a_page_of_another_site_cannot_change_the_server() {
    let (commands, mut received) = tokio::sync::mpsc::channel(1);
    let api = Api::build(move |s| s.with_permissions(allow_all()).with_commands(commands));
    api.factory.add("cam1", Script::video(Codec::H264, 10_000));
    api.hub.upsert("cam", &["fake:cam1"]).unwrap();
    let evil: &[u8] = b"http://evil.example";
    for (method, uri) in [
        (Method::POST, "/api/restart"),
        (Method::POST, "/api/exit?code=0"),
        (Method::POST, "/api/config"),
        (Method::PATCH, "/api/config"),
        (Method::PUT, "/api/streams?name=cam&src=fake:cam2"),
        (Method::PATCH, "/api/streams?name=cam&src=fake:cam2"),
        (Method::DELETE, "/api/streams?src=cam"),
        (Method::DELETE, "/api/log"),
        (Method::PUT, "/api/preload?src=cam"),
        (Method::POST, "/api/webrtc?src=cam"),
    ] {
        for origin in [evil, b"http://127.0.0.1:9999", b"null"] {
            let reply = from(&api, method.clone(), uri, Some(origin), None).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri} from {}", String::from_utf8_lossy(origin));
            assert!(reply.text().contains("api.origin"), "{}", reply.text());
        }
    }
    assert!(received.try_recv().is_err(), "nothing restarted or stopped");
    assert!(api.hub.get("cam").is_some(), "nothing removed");
    // The server's own page, and a program (no Origin), as before.
    let reply = from(&api, Method::POST, "/api/restart", Some(b"http://127.0.0.1:1984"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(received.recv().await.is_some());
    let reply = from(&api, Method::POST, "/api/restart", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Reading is not refused: a page may show a picture, never read it back.
    assert_eq!(from(&api, Method::GET, "/api/streams", Some(evil), Some(b"cross-site")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn api_origin_star_lets_every_page_in_as_it_does_for_the_websocket() {
    let (commands, mut received) = tokio::sync::mpsc::channel(1);
    let api = Api::build(move |s| {
        s.with_permissions(allow_all()).with_commands(commands).with_ws_origin(OriginPolicy::from_config(Some("*")))
    });
    let reply = from(&api, Method::POST, "/api/restart", Some(b"http://evil.example"), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(received.recv().await.is_some());
}

#[tokio::test]
async fn a_get_from_another_site_never_sets_a_stream_from_its_src() {
    let api = Api::build(|s| s.with_snapshots(FakeSnapshotter::new(Ok(bytes::Bytes::from_static(b"\xff\xd8")))));
    api.factory.add("cam1", Script::video(Codec::H264, 10_000));
    // An <img> of another site: no Origin, `Sec-Fetch-Site: cross-site`.
    for site in [&b"cross-site"[..], b"same-site"] {
        let reply = from(&api, Method::GET, "/api/frame.jpeg?src=fake:cam1&name=planted", None, Some(site)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", String::from_utf8_lossy(site));
    }
    assert!(api.hub.streams().is_empty(), "nothing set");
    // The server's own page, or a program: set as go2rtc sets it.
    let reply = from(&api, Method::GET, "/api/frame.jpeg?src=fake:cam1&name=mine", None, Some(b"same-origin")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = from(&api, Method::GET, "/api/frame.jpeg?src=fake:cam1&name=program", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    // An existing stream is still shown to another site's page.
    let reply = from(&api, Method::GET, "/api/frame.jpeg?src=mine", None, Some(b"cross-site")).await;
    assert_eq!(reply.status, StatusCode::OK);
}
