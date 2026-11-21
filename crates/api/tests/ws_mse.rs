//! `mse` on `/api/ws` over real TCP (F3): go2rtc's message sequence (the MIME
//! type, the init segment, one fragment per frame from a keyframe), the
//! sources the codec list chooses, the sound it allows, the cached group of
//! pictures a second reader starts from, go2rtc's error form, the end of the
//! subscription with its socket, and a slow reader that skips to the next
//! keyframe. Fragments are read back with `oxrtc-mp4`'s own test reader.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::ws::{Client, Item, Next, error_of, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, H265_PPS, H265_SPS, H265_VPS, hex};
use oxrtc_mp4::reader::{Init, ReadFragment, read_fragment, read_init};
use serde_json::json;

/// What go2rtc's player sends from headless Edge 157 on this PC.
const EDGE: &str = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
/// A browser without H.265 (Firefox).
const NO_HEVC: &str = "avc1.640029,avc1.64002A,avc1.640033,mp4a.40.2,mp4a.40.5,flac,opus";
/// `trun`'s `sample_is_non_sync_sample`.
const NON_SYNC: u32 = 0x0001_0000;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

fn h265() -> TrackInfo {
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: hex(H265_VPS),
        sps: hex(H265_SPS),
        pps: hex(H265_PPS),
    })
}

fn alaw() -> TrackInfo {
    TrackInfo::audio(Codec::Pcma, 8000, 1)
}

/// `cam`: H.264 and A-law; `layout`: a camera's H.265, then an H.264
/// transcode, as openIVR's `camera_N` lists them; `hevc`: H.265 alone.
fn setup(cam: Script) -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", cam);
    factory.add("camera", Script::tracks(vec![h265()], 10_000));
    factory.add("transcode", Script::tracks(vec![h264()], 10_000));
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    hub.upsert("layout", &["fake:camera", "fake:transcode"]).unwrap();
    hub.upsert("hevc", &["fake:camera"]).unwrap();
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

fn sound_and_picture() -> Script {
    Script::tracks(vec![h264(), alaw()], 10_000)
}

/// Sends `mse` with `list`: the MIME type answered, and the init segment.
async fn start(client: &mut Client, list: &str) -> (String, Init) {
    client.send(json!({"type": "mse", "value": list})).await;
    let answer = client.json().await;
    assert_eq!(answer["type"], "mse", "{answer}");
    let mime = answer["value"].as_str().unwrap().to_owned();
    let init = read_init(&client.binary().await).unwrap();
    (mime, init)
}

/// The next `count` fragments.
async fn fragments(client: &mut Client, init: &Init, count: usize) -> Vec<ReadFragment> {
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        out.push(read_fragment(&client.binary().await, &init.trex).unwrap());
    }
    out
}

/// A fragment's one sample: its track, decode time, duration and whether it
/// is a sync sample.
fn only(fragment: &ReadFragment) -> (u32, u64, u32, bool) {
    assert_eq!(fragment.runs.len(), 1, "one frame per fragment");
    let run = &fragment.runs[0];
    assert_eq!(run.samples.len(), 1, "one frame per fragment");
    let sample = &run.samples[0];
    (run.track_id, sample.decode_time, sample.duration, sample.flags & NON_SYNC == 0)
}

fn consumers(hub: &Hub, stream: &str) -> Vec<oxrtc_core::ConsumerSnapshot> {
    hub.get(stream).unwrap().snapshot().consumers
}

#[tokio::test]
async fn the_mime_type_then_the_init_segment_then_one_fragment_per_frame_from_a_keyframe() {
    let (state, hub) = setup(sound_and_picture());
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let (mime, init) = start(&mut client, EDGE).await;
    assert_eq!(mime, "video/mp4; codecs=\"avc1.64001e,flac\"", "the stream's own codecs; G.711 as FLAC");
    let entries: Vec<[u8; 4]> = init.tracks.iter().map(|t| t.entry).collect();
    assert_eq!(entries, vec![*b"avc1", *b"fLaC"]);
    let got = fragments(&mut client, &init, 30).await;
    assert_eq!(got.iter().map(|f| f.sequence).collect::<Vec<_>>(), (1..=30).collect::<Vec<u32>>());
    let samples: Vec<_> = got.iter().map(only).collect();
    assert_eq!((samples[0].0, samples[0].3), (1, true), "the first fragment is the picture's keyframe");
    assert_eq!(samples[0].1, 0, "the socket's media starts at zero (D40)");
    for track in [1, 2] {
        let times: Vec<u64> = samples.iter().filter(|s| s.0 == track).map(|s| s.1).collect();
        assert!(times.len() > 5, "track {track}: {times:?}");
        assert!(times.windows(2).all(|w| w[0] < w[1]), "track {track} rises: {times:?}");
    }
    let listed = consumers(&hub, "cam");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, "mse/fmp4", "go2rtc's name for it");
    assert_eq!(listed[0].remote, Some(client.local.to_string()));
}

#[tokio::test]
async fn a_second_reader_starts_from_the_cached_keyframe_not_the_next_one() {
    // Pictures that carry their number, a keyframe every 10.
    let script = Script { video_bytes: 64, ..Script::tracks(vec![h264()], 10_000) };
    let (state, _hub) = setup(script);
    let server = serve(state, ServeLimits::default()).await;
    let mut first = open_ok(server.addr, "?src=cam").await;
    let (_, init) = start(&mut first, EDGE).await;
    // Wait until a keyframe passed, and three pictures after it.
    let mut since_key = None;
    let mut key = Vec::new();
    while since_key != Some(3) {
        let fragment = fragments(&mut first, &init, 1).await.remove(0);
        if only(&fragment).3 {
            (since_key, key) = (Some(0), fragment.runs[0].samples[0].data.clone());
        } else if let Some(n) = since_key.as_mut() {
            *n += 1;
        }
    }
    let mut second = open_ok(server.addr, "?src=cam").await;
    let (_, init) = start(&mut second, EDGE).await;
    let got = fragments(&mut second, &init, 4).await;
    let samples: Vec<_> = got.iter().map(only).collect();
    assert!(samples[0].3, "it starts at a keyframe: {samples:?}");
    assert_eq!(got[0].runs[0].samples[0].data, key, "the cached one, not the next");
    assert_eq!(samples[0].1, 0, "at the socket's zero");
    assert!(!samples[1].3 && !samples[2].3 && !samples[3].3, "then the pictures that followed it: {samples:?}");
}

#[tokio::test]
async fn the_list_chooses_the_source_as_a_webrtc_offer_does() {
    let (state, hub) = setup(sound_and_picture());
    let server = serve(state, ServeLimits::default()).await;
    // Picture codecs only: a list with sound would have the core start the
    // transcode to learn whether it has any (these fake sources declare
    // nothing), as for a WebRTC offer.
    let mut client = open_ok(server.addr, "?src=layout").await;
    let (mime, init) = start(&mut client, "avc1.640029,hvc1.1.6.L153.B0").await;
    assert!(
        mime.starts_with("video/mp4; codecs=\"hvc1."),
        "the camera's own H.265 for a browser that lists it: {mime}"
    );
    assert_eq!(init.tracks[0].entry, *b"hvc1");
    let sources = hub.get("layout").unwrap().snapshot().sources;
    assert!(sources[0].demand > 0 && sources[1].demand == 0, "the transcode is not started");
    drop(client);

    let mut client = open_ok(server.addr, "?src=layout").await;
    let (mime, _) = start(&mut client, NO_HEVC).await;
    // (EDGE's list with its H.265 would take the camera.)
    assert_eq!(mime, "video/mp4; codecs=\"avc1.64001e\"", "the H.264 transcode otherwise");
    drop(client);

    let mut client = open_ok(server.addr, "?src=layout").await;
    let (mime, init) = start(&mut client, "hev1.1.6.L153.B0").await;
    assert!(mime.starts_with("video/mp4; codecs=\"hev1."), "{mime}");
    assert_eq!(init.tracks[0].entry, *b"hev1");
}

#[tokio::test]
async fn no_sound_when_the_list_takes_none() {
    let (state, _hub) = setup(sound_and_picture());
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    // Opus only: the camera's A-law needs `flac`.
    let (mime, init) = start(&mut client, "avc1.640029,opus").await;
    assert_eq!(mime, "video/mp4; codecs=\"avc1.64001e\"");
    assert_eq!(init.tracks.len(), 1);
    assert!(fragments(&mut client, &init, 10).await.iter().all(|f| only(f).0 == 1));
}

#[tokio::test]
async fn failures_use_go2rtcs_error_form_and_keep_the_socket() {
    let (state, _hub) = setup(sound_and_picture());
    let server = serve(state, ServeLimits::default()).await;
    let mut unknown = open_ok(server.addr, "?src=nope").await;
    unknown.send(json!({"type": "mse", "value": EDGE})).await;
    assert_eq!(unknown.json().await, json!({"type": "error", "value": "mse: stream not found"}));

    let mut client = open_ok(server.addr, "?src=hevc").await;
    client.send(json!({"type": "mse", "value": "vp09.00.10.08"})).await;
    assert_eq!(error_of(&client.json().await), "mse: codecs not matched", "a list with nothing this server sends");
    client.send(json!({"type": "mse", "value": "avc1.640029,mp4a.40.2"})).await;
    assert_eq!(error_of(&client.json().await), "mse: codecs not matched", "an H.265 stream to a list without it");
    // The socket goes on: the player's next mode.
    let (mime, init) = start(&mut client, EDGE).await;
    assert!(mime.contains("hvc1."), "{mime}");
    client.send(json!({"type": "mse", "value": EDGE})).await;
    loop {
        match client.item(Duration::from_secs(5)).await {
            Item::Binary(_) => {}
            Item::Json(reply) => {
                assert_eq!(error_of(&reply), "mse: already streaming on this socket");
                break;
            }
            Item::Over(over) => panic!("{over:?}"),
        }
    }
    assert!(!fragments(&mut client, &init, 3).await.is_empty(), "the first one goes on");
}

#[tokio::test]
async fn closing_the_socket_ends_the_subscription_at_once() {
    let (state, hub) = setup(sound_and_picture());
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let (_, init) = start(&mut client, EDGE).await;
    fragments(&mut client, &init, 3).await;
    assert_eq!(consumers(&hub, "cam").len(), 1);
    client.ws.close(None).await.unwrap();
    drop(client);
    assert!(
        eventually(Duration::from_millis(300), || consumers(&hub, "cam").is_empty()).await,
        "the consumer left with its socket"
    );
    // A socket dropped without a close frame ends it as well.
    let mut client = open_ok(server.addr, "?src=cam").await;
    start(&mut client, EDGE).await;
    drop(client);
    assert!(eventually(Duration::from_millis(300), || consumers(&hub, "cam").is_empty()).await);
}

#[tokio::test]
async fn a_fragment_larger_than_a_message_is_written_whole() {
    // A camera's keyframe of 1 MiB: the socket's writer held 256 KiB.
    let script =
        Script { video_bytes: 1024 * 1024, interval: Duration::from_millis(40), ..Script::tracks(vec![h264()], 100) };
    let (state, _hub) = setup(script);
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let (_, init) = start(&mut client, EDGE).await;
    for fragment in fragments(&mut client, &init, 3).await {
        assert_eq!(fragment.runs[0].samples[0].data.len(), 1024 * 1024 + 4);
    }
}

#[tokio::test]
async fn a_slow_reader_skips_to_the_next_keyframe_and_says_so() {
    let (logs, _guard) = Logs::capture();
    // 200 KB pictures every 5 ms, a keyframe every 10: 40 MB/s.
    let script =
        Script { video_bytes: 200_000, interval: Duration::from_millis(5), ..Script::tracks(vec![h264()], 100_000) };
    let (state, hub) = setup(script);
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let (_, init) = start(&mut client, EDGE).await;
    fragments(&mut client, &init, 5).await;
    // Not read for longer than a live reader's queue holds (2 s), shorter
    // than the write deadline (5 s).
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let got: Vec<_> = fragments(&mut client, &init, 400).await.iter().map(only).collect();
    let dropped = consumers(&hub, "cam")[0].dropped;
    assert!(dropped > 0, "the stream's listing counts the loss");
    // Decode times stay contiguous (D38): the frame after the loss is placed
    // where the previous one ended and lasts until its own time is reached.
    let step = 3600 * 2;
    let resumed: Vec<_> = got.iter().filter(|s| s.2 > 10 * step).collect();
    assert!(!resumed.is_empty(), "a loss shows as one long sample: {:?}", &got[..10]);
    assert!(resumed.iter().all(|s| s.3), "each one a keyframe: {resumed:?}");
    assert!(
        eventually(Duration::from_secs(2), || logs.text().contains("mse reader fell behind the stream")).await,
        "the log says so"
    );
    let next = client.item(Duration::from_millis(100)).await;
    assert!(!matches!(next, Item::Over(Next::Closed(..) | Next::Ended)), "the socket stays open: {next:?}");
}
