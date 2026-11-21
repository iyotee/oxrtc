//! `/api/ws` on the running server, the real path: a str0m viewer signals
//! over go2rtc's WebSocket API, in go2rtc's player's dialect and in Home
//! Assistant's, its candidates trickled before and after the answer, and
//! receives the camera's picture from the same WebRTC egress `POST
//! /api/webrtc` uses; the picture goes on after the socket closes, as
//! go2rtc's player needs (it closes the socket once the picture plays).
//! Skipped (and passed) when FFmpeg or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::whep::{Played, Viewer};
use common::*;
use futures_util::{SinkExt, StreamExt};
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";

type Socket = WebSocketStream<TcpStream>;

async fn open(api: SocketAddr, src: &str) -> Socket {
    let stream = TcpStream::connect(api).await.unwrap();
    let mut request = format!("ws://{api}/api/ws?src={src}").into_client_request().unwrap();
    let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{USER}:{PASS}"));
    request.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Basic {basic}")).unwrap());
    tokio_tungstenite::client_async(request, stream).await.unwrap().0
}

async fn send(socket: &mut Socket, value: Value) {
    socket.send(Message::Text(value.to_string().into())).await.unwrap();
}

/// The next text message, within 30 s (a cold camera).
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

/// The answer's SDP in either dialect, and the candidate messages after it.
async fn answer(socket: &mut Socket) -> (String, Vec<String>) {
    let message = next(socket).await;
    let sdp = match message["type"].as_str() {
        Some("webrtc/answer") => message["value"].as_str(),
        Some("webrtc") => message["value"]["sdp"].as_str(),
        _ => panic!("no answer: {message}"),
    }
    .unwrap()
    .to_owned();
    let mut candidates = Vec::new();
    while candidates.len() < sdp.matches("a=candidate:").count() {
        let message = next(socket).await;
        assert_eq!(message["type"], "webrtc/candidate", "{message}");
        candidates.push(message["value"].as_str().unwrap().to_owned());
    }
    (sdp, candidates)
}

#[tokio::test]
async fn a_viewer_signalled_over_the_websocket_plays_the_camera() {
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

    // go2rtc's player: `webrtc/offer`, its candidate in the offer.
    let mut viewer = Viewer::new(false, true);
    let mut socket = open(api, "cam").await;
    send(&mut socket, json!({"type": "webrtc/offer", "value": viewer.offer})).await;
    let (sdp, candidates) = answer(&mut socket).await;
    assert!(!candidates.is_empty(), "the server's candidates are sent as messages too");
    for line in &candidates {
        assert!(sdp.contains(&format!("a={line}")), "a candidate the answer carries: {line}");
    }
    viewer.accept(&sdp);
    let mut played = Played::default();
    viewer.play(api, (USER, PASS), "cam", 20, Duration::from_secs(30), &mut played).await;
    assert!(played.connected && played.video_frames >= 20, "{played:?}");
    assert_eq!(played.consumers.len(), 1, "one session: {:?}", played.consumers);
    // The player closes its socket once the picture plays: the picture goes on.
    socket.close(None).await.unwrap();
    drop(socket);
    viewer.play(api, (USER, PASS), "cam", 60, Duration::from_secs(15), &mut played).await;
    assert!(played.video_frames >= 60, "the session outlives its socket: {played:?}");

    // Home Assistant's dialect, with no candidate in the offer: one trickled
    // before the answer, the end of candidates after it.
    let mut viewer = Viewer::new(false, false);
    let mut socket = open(api, "cam").await;
    let offer = json!({"type": "webrtc", "value": {"type": "offer", "sdp": viewer.offer, "ice_servers": []}});
    send(&mut socket, offer).await;
    let line = viewer.candidate();
    send(&mut socket, json!({"type": "webrtc/candidate", "value": line})).await;
    let (sdp, _) = answer(&mut socket).await;
    send(&mut socket, json!({"type": "webrtc/candidate", "value": ""})).await;
    viewer.accept(&sdp);
    let mut played = Played::default();
    viewer.play(api, (USER, PASS), "cam", 20, Duration::from_secs(30), &mut played).await;
    assert!(played.connected && played.video_frames >= 20, "{played:?}");
    drop(viewer);

    // An unknown stream: go2rtc's words, and the socket stays.
    send(&mut socket, json!({"type": "webrtc/offer", "value": Viewer::new(false, true).offer})).await;
    let mut other = open(api, "nope").await;
    send(&mut other, json!({"type": "webrtc/offer", "value": Viewer::new(false, true).offer})).await;
    assert_eq!(next(&mut other).await, json!({"type": "error", "value": "webrtc/offer: stream not found"}));
    // The second offer on the first socket is answered too.
    assert_eq!(answer(&mut socket).await.0.lines().next(), Some("v=0"));
    drop((socket, other));
    camera.stop().await;
    assert!(app.shutdown().await);
}
