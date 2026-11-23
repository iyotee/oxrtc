//! `mse` on `/api/ws` follows WebRTC's H.265 rule (D44, the owner's choice of
//! 2026-11-23): the camera's own H.265 goes to an MSE reader only when an
//! `hvc1`/`hev1` entry of its codec list takes the camera's profile, tier and
//! level; otherwise the stream's H.264 transcode, which never starts for a
//! reader that takes the camera. openIVR's layout (the camera, then its
//! transcode), fake sources over real TCP.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;

use common::ws::{Client, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, H265_PPS, H265_SPS, H265_VPS, hex};
use oxrtc_mp4::reader::read_init;
use serde_json::json;

/// What go2rtc's player sends from headless Edge 157 on this PC: its only
/// H.265 entry is Main at level 5.1.
const EDGE: &str = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
/// libx265's Main 10, 640x360, level 2.1.
const MAIN10_L21: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

/// [`H265_SPS`] (Main, level 4) at level 6.2.
fn main_l62() -> String {
    H265_SPS.replace("030078a0", "0300baa0")
}

fn h265(sps: &str) -> TrackInfo {
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: hex(H265_VPS),
        sps: hex(sps),
        pps: hex(H265_PPS),
    })
}

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

/// `main`, `main10`, `level62`: a camera of that SPS, then its transcode.
fn setup() -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("main", Script::tracks(vec![h265(H265_SPS)], 10_000));
    factory.add("main10", Script::tracks(vec![h265(MAIN10_L21)], 10_000));
    factory.add("level62", Script::tracks(vec![h265(&main_l62())], 10_000));
    factory.add("transcode", Script::tracks(vec![h264()], 10_000));
    let hub = hub(factory);
    for camera in ["main", "main10", "level62"] {
        hub.upsert(camera, &[&format!("fake:{camera}"), "fake:transcode"]).unwrap();
    }
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

/// The frames the transcode of `camera` ever published.
fn transcoded(hub: &Hub, camera: &str) -> u64 {
    hub.get(camera).unwrap().snapshot().sources[1].frames
}

/// The MIME type answered to `list`, and the init segment's video codec.
async fn played(client: &mut Client, list: &str) -> (String, Codec) {
    client.send(json!({"type": "mse", "value": list})).await;
    let answer = client.json().await;
    assert_eq!(answer["type"], "mse", "{answer}");
    let init = read_init(&client.binary().await).unwrap();
    let video = init.tracks.iter().find(|t| &t.handler == b"vide").unwrap();
    let codec = if &video.entry == b"avc1" { Codec::H264 } else { Codec::H265 };
    (answer["value"].as_str().unwrap().to_owned(), codec)
}

#[tokio::test]
async fn go2rtc_s_player_gets_a_main_camera_s_h265_and_the_transcode_never_starts() {
    let (state, hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=main").await;
    let (mime, codec) = played(&mut client, EDGE).await;
    assert_eq!(codec, Codec::H265, "{mime}");
    assert!(mime.contains("hvc1.1.6.L120"), "the camera's own codec string: {mime}");
    assert_eq!(transcoded(&hub, "main"), 0, "no transcode for a reader that takes the camera");
}

#[tokio::test]
async fn go2rtc_s_player_gets_the_transcode_for_a_main10_or_a_level_6_2_camera() {
    for camera in ["main10", "level62"] {
        let (state, hub) = setup();
        let server = serve(state, ServeLimits::default()).await;
        let mut client = open_ok(server.addr, &format!("?src={camera}")).await;
        let (mime, codec) = played(&mut client, EDGE).await;
        assert_eq!(codec, Codec::H264, "{camera}: {mime}");
        assert!(mime.contains("avc1."), "{camera}: {mime}");
        let runs = eventually(std::time::Duration::from_secs(3), || transcoded(&hub, camera) > 0).await;
        assert!(runs, "{camera}: the transcode runs");
    }
}

#[tokio::test]
async fn a_list_with_a_main10_entry_gets_the_main10_camera_s_h265() {
    let (state, hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=main10").await;
    let (mime, codec) = played(&mut client, "avc1.640029,hvc1.1.6.L153.B0,hvc1.2.4.L153.B0,mp4a.40.2").await;
    assert_eq!(codec, Codec::H265, "{mime}");
    assert!(mime.contains("hvc1.2.4.L63"), "{mime}");
    assert_eq!(transcoded(&hub, "main10"), 0);
    // An entry with no level takes the level 6.2 camera (no ceiling, D41).
    let mut client = open_ok(server.addr, "?src=level62").await;
    let (mime, codec) = played(&mut client, "avc1.640029,hvc1.1.6").await;
    assert_eq!(codec, Codec::H265, "{mime}");
}
