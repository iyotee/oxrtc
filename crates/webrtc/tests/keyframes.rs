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

/// A viewer that asks again soon after a replay was not brought back by it
/// (a browser behind, W3): it is left to the camera's next keyframe, the
/// picture stopped until then; a request after that window is a new loss,
/// replayed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_that_asks_again_soon_after_a_replay_waits_for_the_camera() {
    let (_server, mut client) = playing().await;
    pictures(&mut client, Duration::from_millis(300)).await;
    client.act(Action::RequestKeyframe).await;
    let first = pictures(&mut client, Duration::from_millis(700)).await;
    let replayed = first.iter().find(|(key, ..)| *key).expect("the first request is answered at once").2;
    // A browser repeats its request while it waits: five more at once.
    for _ in 0..5 {
        client.act(Action::RequestKeyframe).await;
    }
    let asked = Instant::now();
    let later = pictures(&mut client, Duration::from_millis(1200)).await;
    let sent_after = later.iter().filter(|(_, _, at)| at.duration_since(asked) > Duration::from_millis(100)).count();
    assert_eq!(sent_after, 0, "the picture waits for the camera's keyframe (23 s away): {sent_after} frames came");
    // Past the window: a new loss, answered by the cached group again.
    let window = Duration::from_millis(2100).saturating_sub(replayed.elapsed());
    tokio::time::sleep(window).await;
    client.act(Action::RequestKeyframe).await;
    let again = pictures(&mut client, Duration::from_millis(800)).await;
    assert!(again.iter().any(|(key, ..)| *key), "a later request is replayed: {} frames", again.len());
}

/// A camera that sends a keyframe a second (the bench's): a request is left
/// to its next keyframe - nothing replayed, nothing sent before it, which
/// the viewer could not decode (W3: a replay to a browser that asked
/// because it was behind is a burst it drops).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_near_the_cameras_keyframe_is_answered_by_that_keyframe() {
    let (params, frames) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, frames, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), Duration::from_secs(5)).await;
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    let keyframe = |e: &ClientEvent| matches!(e, ClientEvent::Media { kind: RtcKind::Video, keyframe: true, .. });
    // Two keyframes seen: the session knows the camera's interval. The
    // request comes just after the second, so the third is about 1 s away.
    for n in 0..2 {
        assert!(client.wait_for(Duration::from_secs(5), keyframe).await.is_some(), "keyframe {n}");
    }
    let asked = Instant::now();
    client.act(Action::RequestKeyframe).await;
    let after = pictures(&mut client, Duration::from_millis(1600)).await;
    let key = after.iter().position(|(key, ..)| *key).expect("the camera's next keyframe");
    let late_deltas = after[..key].iter().filter(|(_, _, at)| at.duration_since(asked) > Duration::from_millis(100));
    assert_eq!(late_deltas.count(), 0, "frames sent after the request, before the keyframe");
    let waited = after[key].2.duration_since(asked);
    assert!(waited > Duration::from_millis(500), "a keyframe {waited:?} after the request: replayed, not the camera's");
    assert!(waited < Duration::from_millis(1300), "the camera's keyframe came {waited:?} after the request");
    let times: Vec<u64> = after[key..].iter().map(|(_, time, _)| *time).collect();
    assert!(times.windows(2).all(|w| w[1] > w[0]), "times: {times:?}");
    assert!(times.len() > 5, "the picture goes on after the keyframe: {}", times.len());
    drop((server, hub));
}

/// A session that connects later than its live budget finds its queue cut,
/// with no keyframe left in it: it resumes from the cached group of
/// pictures at once, not at the camera's next keyframe (23 s away here, 6 s
/// on the owner's cameras), and never with nothing to show.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_connects_past_its_budget_resumes_from_the_cached_gop() {
    let limits = oxrtc_core::Limits {
        live_queue: oxrtc_core::QueueBudget { max_bytes: 8 * 1024 * 1024, max_age: Duration::from_millis(300) },
        ..oxrtc_core::Limits::default()
    };
    let hub = hub_with(FakeFactory::default().with("cam", long_gop()), limits);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), Duration::from_secs(5)).await;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client =
        connect_late(&server, stream.clone(), browser(false), socket, Duration::from_millis(600)).await.unwrap();
    let after = pictures(&mut client, Duration::from_secs(3)).await;
    let key = after.iter().position(|(key, ..)| *key).expect("no keyframe at all");
    let shown = after.len() - key;
    // About 2.4 s at 25 frames a second once connected.
    assert!(shown > 30, "{shown} pictures from the keyframe on: the session waits for the camera's next one");
    let times: Vec<u64> = after[key..].iter().map(|(_, time, _)| *time).collect();
    assert!(times.windows(2).all(|w| w[1] > w[0]), "times: {times:?}");
    assert!(stream.snapshot().consumers.iter().any(|c| c.dropped > 0), "the queue was cut, as the test means");
    drop((server, hub));
}
