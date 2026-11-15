//! A viewer's keyframe request is answered from the cached group of
//! pictures (R24): the camera's next keyframe may be many seconds away, and
//! oxrtc has no upstream to ask.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};
use common::*;
use oxrtc_core::Codec;
use str0m::format::Codec as RtcCodec;
use str0m::media::MediaKind as RtcKind;

/// A group of pictures of 600 frames (about 23 s at this script's pace):
/// no keyframe of the camera's own comes during a test.
fn long_gop() -> Script {
    let (params, _) = synthetic_h264();
    let frames = (0..600)
        .map(|i| {
            let key = i == 0;
            let mut nal = vec![if key { 0x65 } else { 0x41 }, 0x88];
            nal.extend(std::iter::repeat_n(i as u8, if key { 3000 } else { 400 }));
            let mut data = BytesMut::new();
            data.put_u32(nal.len() as u32);
            data.put_slice(&nal);
            ScriptFrame { track: 0, keyframe: key, data: data.freeze() }
        })
        .collect();
    av_script(Codec::H264, params, frames, false)
}

/// The picture frames the client receives within `within`: (keyframe, RTP
/// time, when).
async fn pictures(client: &mut Client, within: Duration) -> Vec<(bool, u64, Instant)> {
    let deadline = tokio::time::Instant::now() + within;
    let mut out = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, client.events.recv()).await {
        if let ClientEvent::Media { kind: RtcKind::Video, codec: RtcCodec::H264, keyframe, time, at, .. } = event {
            out.push((keyframe, time, at));
        }
    }
    out
}

async fn playing() -> (Running, Client) {
    let hub = hub(FakeFactory::default().with("cam", long_gop()));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), Duration::from_secs(5)).await;
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    // The session starts on the cached group's keyframe.
    let first = client
        .wait_for(Duration::from_secs(5), |e| {
            matches!(e, ClientEvent::Media { kind: RtcKind::Video, keyframe: true, .. })
        })
        .await;
    assert!(first.is_some(), "no first keyframe");
    (Running { _server: server, _hub: hub }, client)
}

/// Keeps the server and its hub alive for the test.
struct Running {
    _server: oxrtc_webrtc::WebRtcServer,
    _hub: std::sync::Arc<oxrtc_core::Hub>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyframe_request_after_a_loss_is_answered_from_the_cached_gop() {
    let (_server, mut client) = playing().await;
    let before = pictures(&mut client, Duration::from_millis(500)).await;
    assert!(!before.is_empty() && before.iter().all(|(key, ..)| !key), "deltas only after the first keyframe");
    // A loss on the way: the decoder has a hole, the browser asks.
    client.act(Action::Blackout(Duration::from_millis(300))).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let asked = Instant::now();
    client.act(Action::RequestKeyframe).await;
    let after = pictures(&mut client, Duration::from_secs(2)).await;
    let key = after.iter().position(|(key, ..)| *key).expect("no keyframe after the request: nothing answered it");
    let (_, key_time, key_at) = after[key];
    assert!(key_at.duration_since(asked) < Duration::from_millis(800), "answered {:?} later", key_at - asked);
    let last_before = before.last().unwrap().1;
    assert!(key_time > last_before, "the replay goes back in time: {key_time} after {last_before}");
    // Its group follows it, then the camera's frames, never back in time.
    let times: Vec<u64> = after[key..].iter().map(|(_, time, _)| *time).collect();
    assert!(times.windows(2).all(|w| w[1] > w[0]), "times after the replay: {times:?}");
    assert!(after[key + 1..].iter().all(|(key, ..)| !key), "one keyframe per request");
    assert!(times.len() > 20, "the group and the frames after it: {}", times.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyframe_requests_are_answered_at_most_once_a_second() {
    let (_server, mut client) = playing().await;
    pictures(&mut client, Duration::from_millis(300)).await;
    client.act(Action::RequestKeyframe).await;
    let first = pictures(&mut client, Duration::from_millis(700)).await;
    let replayed = first.iter().find(|(key, ..)| *key).expect("the first request is answered at once").2;
    // A browser repeats its request while it waits: five more at once.
    for _ in 0..5 {
        client.act(Action::RequestKeyframe).await;
    }
    let mut later = pictures(&mut client, Duration::from_millis(1500)).await;
    later.retain(|(key, ..)| *key);
    assert_eq!(later.len(), 1, "five requests within the interval are answered once: {}", later.len());
    let gap = later[0].2.duration_since(replayed);
    assert!(gap >= Duration::from_millis(900), "the second replay came {gap:?} after the first");
}
