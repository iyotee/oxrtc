//! `mp4` on `/api/ws` over real TCP (F2): go2rtc's poster mode - its player,
//! with neither MSE nor native HLS, asks `{"type":"mp4","value":"<codecs>"}`,
//! is answered `{"type":"mse","value":"<MIME type>"}` as go2rtc answers it,
//! then one binary message per keyframe of the video, each a whole MP4 file
//! (the init segment and that keyframe's fragment) it sets as a `<video>`'s
//! `data:` URL. The files come from MSE's one fMP4 path; no sound; go2rtc's
//! error form before the answer; one stream per socket; the end of the
//! subscription with its socket and of the socket with its stream.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::ws::{Item, Next, error_of, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::host_memory::{Fixed, HostReading, MemorySource};
use oxrtc_core::memory::{BudgetSize, MemoryBudget};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use oxrtc_mp4::reader::{atoms, read_fragment, read_init};
use serde_json::json;

/// What go2rtc's player's `codecs(canPlayType)` lists in headless Edge.
const EDGE: &str = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
/// `trun`'s `sample_is_non_sync_sample`.
const NON_SYNC: u32 = 0x0001_0000;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

/// A message as its init segment and its one fragment, as a file holds them.
fn split(file: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let top = atoms(file, 0).unwrap();
    let kinds: Vec<[u8; 4]> = top.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, vec![*b"ftyp", *b"moov", *b"moof", *b"mdat"], "a whole file: one init and one fragment");
    ([top[0].whole, top[1].whole].concat(), [top[2].whole, top[3].whole].concat())
}

/// `cam`: H.264 and A-law, a keyframe every 10 pictures; `few`: 40 frames.
fn setup() -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::tracks(vec![h264(), TrackInfo::audio(Codec::Pcma, 8000, 1)], 10_000));
    factory.add("few", Script::tracks(vec![h264()], 40));
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    hub.upsert("few", &["fake:few"]).unwrap();
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

#[tokio::test]
async fn go2rtcs_answer_then_one_whole_file_per_keyframe() {
    let (state, hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "mp4", "value": EDGE})).await;
    // go2rtc answers an `mp4` with an `mse` message: the files' type, the
    // picture alone though the list names sound.
    let told = client.json().await;
    assert_eq!(told["type"], "mse", "{told}");
    let mime = told["value"].as_str().unwrap();
    assert!(mime.starts_with("video/mp4; codecs=\"avc1.") && !mime.contains(','), "{mime}");
    let mut times = Vec::new();
    for _ in 0..4 {
        let file = client.binary().await;
        // Each message is a file a player opens alone: an init segment with
        // the picture's one track, then one fragment of one sync sample.
        let (init, fragment) = split(&file);
        let init = read_init(&init).unwrap();
        assert_eq!(init.tracks.len(), 1, "the picture alone");
        let fragment = read_fragment(&fragment, &init.trex).unwrap();
        assert_eq!(fragment.runs.len(), 1);
        let samples = &fragment.runs[0].samples;
        assert_eq!(samples.len(), 1, "one keyframe a file");
        assert_eq!(samples[0].flags & NON_SYNC, 0, "a keyframe");
        times.push(samples[0].decode_time);
    }
    // Every keyframe, none left out: the fake camera's come at one pace.
    let step = times[1] - times[0];
    assert!(step > 0 && times.windows(2).all(|w| w[1] - w[0] == step), "{times:?}");
    let consumers = hub.get("cam").unwrap().snapshot().consumers;
    assert_eq!(consumers.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(), ["mp4"]);
    // One stream per socket.
    client.send(json!({"type": "mp4"})).await;
    let mut refused = None;
    for _ in 0..50 {
        if let Item::Json(value) = client.item(Duration::from_secs(5)).await {
            refused = Some(value);
            break;
        }
    }
    assert_eq!(error_of(&refused.unwrap()), "mp4: already streaming on this socket");
    drop(client);
    assert!(
        eventually(Duration::from_millis(500), || hub.get("cam").unwrap().snapshot().consumers.is_empty()).await,
        "its socket gone, the subscription ends"
    );
}

#[tokio::test]
async fn an_error_before_the_answer_leaves_the_socket_for_the_next_mode() {
    let (state, _hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=nope").await;
    client.send(json!({"type": "mp4", "value": EDGE})).await;
    assert_eq!(error_of(&client.json().await), "mp4: stream not found");
    let mut client = open_ok(server.addr, "?src=cam").await;
    // A list with no picture in it: go2rtc's words.
    client.send(json!({"type": "mp4", "value": "mp4a.40.2,opus"})).await;
    assert_eq!(error_of(&client.json().await), "mp4: codecs not matched");
    // The socket stays: no value is go2rtc's default, H.264 or H.265.
    client.send(json!({"type": "mp4"})).await;
    assert_eq!(client.json().await["type"], "mse");
    assert!(read_init(&split(&client.binary().await).0).is_ok());
}

#[tokio::test]
async fn it_counts_with_the_offers_of_its_socket() {
    let (state, hub) = setup();
    let limits = oxrtc_api::WsLimits { max_offers: 1, ..oxrtc_api::WsLimits::default() };
    let server = serve(state.with_ws_limits(limits), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "mp4"})).await;
    assert_eq!(client.json().await["type"], "mse");
    client.binary().await;
    // The stream is replaced after the answer: its readers end, the player
    // is told and the socket closed, so that it starts again.
    hub.upsert("cam", &["fake:few"]).unwrap();
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
    assert!(error_of(&told.unwrap()).starts_with("mp4: streams:"));
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "mp4"})).await;
    assert_eq!(client.json().await["type"], "mse");
    client.send(json!({"type": "mse"})).await;
    let mut refused = None;
    for _ in 0..50 {
        if let Item::Json(value) = client.item(Duration::from_secs(5)).await {
            refused = Some(value);
            break;
        }
    }
    assert_eq!(error_of(&refused.unwrap()), "mse: already streaming on this socket");
}

#[tokio::test]
async fn it_reads_within_the_output_memory_budget() {
    // Item D's budget (D45): an `mp4` socket is one of its readers, as an
    // `mse` one, gone with its socket.
    let (state, hub) = setup();
    let reading = HostReading { limit: 8 << 30, available: 6 << 30, source: MemorySource::CgroupV2 };
    assert!(hub.set_output_memory(MemoryBudget::new(BudgetSize::Bytes(64 << 20), Box::new(Fixed(reading)))));
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "mp4"})).await;
    assert_eq!(client.json().await["type"], "mse");
    client.binary().await;
    assert_eq!(hub.output_memory().unwrap().snapshot().readers, 1);
    drop(client);
    assert!(eventually(Duration::from_secs(2), || hub.output_memory().unwrap().snapshot().readers == 0).await);
}
