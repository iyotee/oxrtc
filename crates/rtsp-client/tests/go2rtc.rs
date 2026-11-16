//! The driver against a real RTSP server: go2rtc serving FFmpeg's test
//! pattern, on free ports, with RTSP authentication on. Skipped (and passed)
//! when go2rtc or FFmpeg is absent; the paths come from `OXRTC_GO2RTC` and
//! `OXRTC_FFMPEG`.
// Test code: a failed expectation is a panic by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::frame::{NalUnits, h264_is_keyframe, h264_nal_type, h265_is_keyframe, h265_nal_type, validate_avcc};
use oxrtc_core::{Codec, CodecParams, ConsumerTag, Event, MediaKind, Stream, Subscription, TrackInfo, Want};
use tokio::process::{Child, Command};

const DEFAULT_GO2RTC: &str = "K:/LABS/CascadeProjects/bench-bin/go2rtc-1.9.15/go2rtc.exe";
const DEFAULT_FFMPEG: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";

struct Go2rtc {
    child: Child,
    dir: PathBuf,
    rtsp_port: u16,
}

impl Drop for Go2rtc {
    fn drop(&mut self) {
        // Our own child, through its handle; kill_on_drop is the backstop.
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Go2rtc {
    /// Kills go2rtc, waits for it to exit, then removes its directory (retried:
    /// the FFmpeg go2rtc started keeps it as working directory until it notices
    /// its output is gone, a moment later).
    async fn stop(mut self) {
        let _ = self.child.kill().await;
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        eprintln!("could not remove {}", self.dir.display());
    }

    async fn start() -> Option<Self> {
        let go2rtc = std::env::var("OXRTC_GO2RTC").unwrap_or_else(|_| DEFAULT_GO2RTC.into());
        let ffmpeg = std::env::var("OXRTC_FFMPEG").unwrap_or_else(|_| DEFAULT_FFMPEG.into());
        for bin in [&go2rtc, &ffmpeg] {
            if !std::path::Path::new(bin).is_file() {
                eprintln!("skipping: {bin} not found (set OXRTC_GO2RTC / OXRTC_FFMPEG)");
                return None;
            }
        }
        let [api, rtsp, webrtc, srtp] = [(); 4].map(|()| common::free_port());
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("oxrtc-rtsp-client-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = format!(
            "api:\n  listen: \"127.0.0.1:{api}\"\n\
             rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n  username: \"user\"\n  password: \"p@ss\"\n\
             webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n\
             srtp:\n  listen: \"127.0.0.1:{srtp}\"\n\
             ffmpeg:\n  bin: \"{ffmpeg}\"\n\
             streams:\n\
             \x20 h264: \"ffmpeg:virtual?video=testsrc&size=720#video=h264\"\n\
             \x20 h265: \"ffmpeg:virtual?video=testsrc&size=720#video=h265\"\n"
        );
        let path = dir.join("go2rtc.yaml");
        std::fs::write(&path, config).unwrap();
        let child = Command::new(&go2rtc)
            .arg("-config")
            .arg(&path)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let me = Self { child, dir, rtsp_port: rtsp };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::net::TcpStream::connect(("127.0.0.1", rtsp)).await.is_err() {
            assert!(tokio::time::Instant::now() < deadline, "go2rtc did not listen on {rtsp}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Some(me)
    }

    fn url(&self, userinfo: &str, path: &str) -> String {
        format!("rtsp://{userinfo}127.0.0.1:{}/{path}#timeout=10", self.rtsp_port)
    }
}

async fn subscribe(stream: &Arc<Stream>, want: &Want) -> Subscription {
    tokio::time::timeout(Duration::from_secs(30), stream.subscribe(want, ConsumerTag::new("test")))
        .await
        .expect("subscribe timed out")
        .expect("subscribe failed")
}

async fn next(sub: &mut Subscription) -> Event {
    tokio::time::timeout(Duration::from_secs(20), sub.recv()).await.expect("no event within 20 s").expect("closed")
}

type Check = fn(&[u8]) -> bool;
type NalType = fn(&[u8]) -> Option<u8>;

/// Reads `frames` video frames and checks what the driver promises.
async fn check_video(sub: &mut Subscription, codec: Codec, frames: usize) -> Arc<[TrackInfo]> {
    let Event::Tracks(mut tracks) = next(sub).await else { panic!("first event is not Tracks") };
    assert_eq!((tracks[0].kind, tracks[0].codec), (MediaKind::Video, codec), "{tracks:?}");
    assert_eq!(tracks[0].clock_rate, 90_000);
    let (is_key, nal_type, param_types): (Check, NalType, &[u8]) = match codec {
        Codec::H264 => (h264_is_keyframe, h264_nal_type, &[7, 8]),
        _ => (h265_is_keyframe, h265_nal_type, &[32, 33, 34]),
    };
    let (mut seen, mut keyframes, mut last_pts) = (0, 0, i64::MIN);
    while seen < frames {
        match next(sub).await {
            Event::Tracks(t) => tracks = t,
            Event::Discontinuity => panic!("discontinuity while reading at full speed"),
            Event::Frame { track, frame } => {
                if tracks[track].kind != MediaKind::Video {
                    continue;
                }
                validate_avcc(&frame.data).expect("not AVCC");
                if seen == 0 {
                    assert!(frame.keyframe, "first video frame is not a keyframe");
                }
                if frame.keyframe {
                    keyframes += 1;
                    assert!(is_key(&frame.data), "keyframe without an IRAP/IDR NAL");
                    let types: Vec<u8> = NalUnits::new(&frame.data).filter_map(nal_type).collect();
                    for t in param_types {
                        assert!(types.contains(t), "keyframe lacks parameter set {t}: {types:?}");
                    }
                }
                assert!(frame.pts >= last_pts, "pts went back: {} after {last_pts}", frame.pts);
                last_pts = frame.pts;
                seen += 1;
            }
        }
    }
    assert!(keyframes >= 1);
    tracks
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_from_go2rtc_with_credentials_and_an_in_process_relay() {
    let Some(go2rtc) = Go2rtc::start().await else { return };
    // Nothing listens on the relay port: the relay can only work in-process.
    let relay_port = common::free_port();
    let hub = common::hub(vec![relay_port]);
    let camera = hub.upsert("cam", &[go2rtc.url("user:p%40ss@", "h264")]).unwrap();
    let relay = hub.upsert("relay", &[format!("rtsp://127.0.0.1:{relay_port}/cam?video#timeout=20")]).unwrap();

    let mut sub = subscribe(&camera, &Want::all()).await;
    let tracks = check_video(&mut sub, Codec::H264, 60).await;
    assert!(matches!(&tracks[0].params, CodecParams::H264 { sps, pps } if !sps.is_empty() && !pps.is_empty()));

    let mut relayed = subscribe(&relay, &Want::from_query("video").unwrap()).await;
    check_video(&mut relayed, Codec::H264, 10).await;
    let source = &relay.snapshot().sources[0];
    assert!(source.frames >= 10, "{source:?}");

    drop((sub, relayed));
    tokio::time::sleep(Duration::from_millis(500)).await; // TEARDOWN reaches go2rtc, which stops FFmpeg
    go2rtc.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h265_from_go2rtc() {
    let Some(go2rtc) = Go2rtc::start().await else { return };
    let hub = common::hub(vec![]);
    let camera = hub.upsert("cam", &[go2rtc.url("user:p%40ss@", "h265")]).unwrap();
    let mut sub = subscribe(&camera, &Want::from_query("video").unwrap()).await;
    let tracks = check_video(&mut sub, Codec::H265, 30).await;
    assert!(matches!(&tracks[0].params, CodecParams::H265 { vps, .. } if !vps.is_empty()), "{tracks:?}");
    drop(sub);
    tokio::time::sleep(Duration::from_millis(500)).await;
    go2rtc.stop().await;
}

// go2rtc skips RTSP authentication for loopback clients (see docs/DECISIONS.md,
// D7), and tests may only use 127.0.0.1: a wrong password cannot fail here. The
// 401 path is covered against a scripted camera in `fake_camera.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn go2rtc_lets_loopback_clients_in_whatever_the_password() {
    let Some(go2rtc) = Go2rtc::start().await else { return };
    let hub = common::hub(vec![]);
    let camera = hub.upsert("cam", &[go2rtc.url("user:wrong@", "h264")]).unwrap();
    let mut sub = subscribe(&camera, &Want::from_query("video").unwrap()).await;
    check_video(&mut sub, Codec::H264, 5).await;
    drop(sub);
    tokio::time::sleep(Duration::from_millis(500)).await;
    go2rtc.stop().await;
}
