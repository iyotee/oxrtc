//! `mjpeg` on `/api/ws` over real TCP (F5): go2rtc's sequence (`{"type":
//! "mjpeg"}`, then one binary frame per JPEG picture, which go2rtc's player
//! shows as they come), the standard Huffman tables written into a webcam's
//! pictures, go2rtc's error form (an error before the answer leaves the
//! socket open for the player's next mode), one stream per socket, and the
//! end of the subscription with its socket or of the socket with its stream.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::ws::{Item, Next, error_of, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::jpeg::header as jpeg_header;
use oxrtc_core::{Codec, Hub};
use serde_json::json;

/// `webcam`: JPEG pictures every 20 ms; `camera`: H.264 alone; `few`: 5.
fn setup() -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("webcam", Script::video(Codec::Jpeg, 10_000));
    factory.add("camera", Script::video(Codec::H264, 10_000));
    factory.add("few", Script::video(Codec::Jpeg, 5));
    let hub = hub(factory);
    for name in ["webcam", "camera", "few"] {
        hub.upsert(name, &[&format!("fake:{name}")]).unwrap();
    }
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

#[tokio::test]
async fn go2rtcs_answer_then_one_picture_per_binary_frame() {
    let (state, hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=webcam").await;
    // go2rtc's player sends it with no value.
    client.send(json!({"type": "mjpeg"})).await;
    assert_eq!(client.json().await, json!({"type": "mjpeg"}));
    let mut pictures = Vec::new();
    for _ in 0..10 {
        let picture = client.binary().await;
        let read = jpeg_header(&picture).unwrap();
        assert!(read.has_tables && picture.ends_with(&[0xff, 0xd9]), "a whole picture, its tables written in");
        pictures.push(picture);
    }
    assert!(pictures.windows(2).all(|w| w[0] != w[1]));
    let consumers = hub.get("webcam").unwrap().snapshot().consumers;
    assert_eq!(consumers.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(), ["mjpeg"]);
    // One stream per socket.
    client.send(json!({"type": "mjpeg"})).await;
    let mut refused = None;
    for _ in 0..50 {
        if let Item::Json(value) = client.item(Duration::from_secs(5)).await {
            refused = Some(value);
            break;
        }
    }
    assert_eq!(error_of(&refused.unwrap()), "mjpeg: already streaming on this socket");
    drop(client);
    assert!(
        eventually(Duration::from_millis(500), || hub.get("webcam").unwrap().snapshot().consumers.is_empty()).await,
        "its socket gone, the subscription ends"
    );
}

#[tokio::test]
async fn an_error_before_the_answer_leaves_the_socket_for_the_next_mode() {
    let (state, _hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=nope").await;
    client.send(json!({"type": "mjpeg"})).await;
    assert_eq!(error_of(&client.json().await), "mjpeg: stream not found");
    // No JPEG and no FFmpeg to encode one: go2rtc's words.
    let mut client = open_ok(server.addr, "?src=camera").await;
    client.send(json!({"type": "mjpeg"})).await;
    assert_eq!(error_of(&client.json().await), "mjpeg: codecs not matched");
    // The socket stays, and is answered again.
    client.send(json!({"type": "mjpeg"})).await;
    assert_eq!(error_of(&client.json().await), "mjpeg: codecs not matched");
}

#[tokio::test]
async fn the_socket_closes_when_its_stream_ends_after_the_answer() {
    let (state, hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=few").await;
    client.send(json!({"type": "mjpeg"})).await;
    assert_eq!(client.json().await, json!({"type": "mjpeg"}));
    for _ in 0..5 {
        client.binary().await;
    }
    // The stream is replaced: its readers end, and the player is told.
    hub.upsert("few", &["fake:webcam"]).unwrap();
    let mut told = None;
    loop {
        match client.item(Duration::from_secs(5)).await {
            Item::Json(value) => told = Some(value),
            Item::Binary(_) => {}
            Item::Over(over) => {
                assert!(matches!(over, Next::Closed(1000, _)), "{over:?}");
                break;
            }
        }
    }
    assert!(error_of(&told.unwrap()).starts_with("mjpeg: streams:"));
}
