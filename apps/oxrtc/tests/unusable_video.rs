//! An offer whose video lists VP8 alone, on the running server over the real
//! path (go2rtc serving FFmpeg's H.264 test pattern as the camera, D43): `POST
//! /api/webrtc` and `/api/ws` refuse it by the same call, with the same
//! status and words, naming the stream's codecs and the offer's; an H.264
//! offer on the same socket is still answered. Skipped (and passed) when
//! FFmpeg or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::whep::Viewer;
use common::*;
use futures_util::{SinkExt, StreamExt};
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::{Value, json};
use str0m::RtcConfig;
use str0m::media::{Direction, MediaKind};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";

/// A VP8 and Opus offer, as a VP8-only client writes it.
fn vp8_offer() -> String {
    let mut rtc = RtcConfig::new().clear_codecs().enable_vp8(true).enable_opus(true, false).build(Instant::now());
    let mut sdp = rtc.sdp_api();
    sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    sdp.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    sdp.apply().unwrap().0.to_sdp_string()
}

type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

async fn open(api: SocketAddr, src: &str) -> Socket {
    let stream = TcpStream::connect(api).await.unwrap();
    let mut request = format!("ws://{api}/api/ws?src={src}").into_client_request().unwrap();
    let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{USER}:{PASS}"));
    request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Basic {basic}")).unwrap());
    tokio_tungstenite::client_async(request, stream).await.unwrap().0
}

async fn next(socket: &mut Socket) -> Value {
    loop {
        let item = tokio::time::timeout(Duration::from_secs(30), socket.next()).await.expect("a message");
        match item.expect("the socket is open").unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_vp8_offer_is_refused_the_same_way_over_whep_and_the_websocket() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start(&tools).await;
    let text = format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n  candidates: []\n\
         streams:\n  cam: \"{}\"\n",
        camera.url("h264")
    );
    let app = App::start(&parse(&text).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;

    let reply = Request::new("POST", "/api/webrtc?src=cam")
        .auth(USER, PASS)
        .body("application/sdp", vp8_offer().as_bytes())
        .send(api)
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    let said = reply.text().trim().to_owned();
    assert_eq!(said, "streams: codecs not matched: video:H264 => video:VP8, audio:OPUS");

    let mut socket = open(api, "cam").await;
    socket.send(Message::Text(json!({"type": "webrtc/offer", "value": vp8_offer()}).to_string().into())).await.unwrap();
    assert_eq!(next(&mut socket).await, json!({"type": "error", "value": format!("webrtc/offer: {said}")}));
    // The socket stays, and an H.264 offer on it is answered.
    let viewer = Viewer::new(true, true);
    socket
        .send(Message::Text(json!({"type": "webrtc/offer", "value": viewer.offer}).to_string().into()))
        .await
        .unwrap();
    let answer = next(&mut socket).await;
    assert_eq!(answer["type"], "webrtc/answer", "{answer}");
    assert!(answer["value"].as_str().unwrap().contains("H264/90000"));
    drop((socket, viewer));
    camera.stop().await;
    assert!(app.shutdown().await);
}
