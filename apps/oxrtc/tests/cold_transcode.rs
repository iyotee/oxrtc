//! A viewer that starts a transcode cold (nothing preloaded, as the first
//! viewer of a camera whose browser opens the live view) is shown the picture
//! from the transcode's first keyframe, not from a picture in the middle of
//! its group (found on the owner's MJPEG webcams: the first keyframe a viewer
//! saw was the 27th picture). Skipped (and passed) when FFmpeg, ffprobe or
//! go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::launch::{API_PASS, API_USER, Ports};
use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_viewer_that_starts_a_transcode_cold_gets_its_first_keyframe_first() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start(&tools).await;
    let dir = temp_dir("cold");
    let ports = Ports::free();
    let config = launch::transcode_config(&dir, &tools, &camera, &ports);
    let oxrtc = Oxrtc::start(&config);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let played =
        whep::play_as(ports.api(), (API_USER, API_PASS), "cam_small", None, false, 20, Duration::from_secs(30)).await;
    assert_eq!(played.status, 201, "{played:?}");
    assert!(played.connected && played.video_frames >= 20, "{played:?}");
    assert_eq!(played.first_keyframe_at, Some(0), "the first picture a cold viewer is shown is a keyframe: {played:?}");

    drop(oxrtc);
    camera.stop().await;
    remove_dir(&dir).await;
}

/// The same on an MJPEG webcam through openIVR's own `h264` template (the
/// picture decoded, then encoded with a 30-picture group).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_viewer_of_a_webcam_that_starts_cold_gets_its_first_keyframe_first() {
    let Some(tools) = Tools::find() else { return };
    let dir = temp_dir("cold-webcam");
    let frames = webcam::pictures(&tools, &dir, "default").await;
    let camera = webcam::Webcam::start(frames).await;
    let ports = Ports::free();
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{API_USER}\"\n  password: \"{API_PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n  h264: -codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency \
         -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough\n\
         log:\n  level: info\n\
         streams:\n\
         \x20 cam_src: [\"http://{user}:{pass}@127.0.0.1:{port}/video.mjpg\"]\n\
         \x20 cam: [\"rtsp://127.0.0.1:{rtsp}/cam_src?video#timeout=20\", \
         \"ffmpeg:cam_src#video=h264#width=1920#height=1080#timeout=20\"]\n",
        api = ports.api,
        rtsp = ports.rtsp,
        webrtc = ports.webrtc,
        ffmpeg = tools.ffmpeg.display(),
        user = webcam::CAMERA_USER,
        pass = webcam::CAMERA_PASS,
        port = camera.port,
    );
    let config = dir.join("oxrtc.yaml");
    std::fs::write(&config, text).unwrap();
    let oxrtc = Oxrtc::start(&config);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let played =
        whep::play_as(ports.api(), (API_USER, API_PASS), "cam", None, false, 40, Duration::from_secs(40)).await;
    assert_eq!(played.status, 201, "{played:?}");
    assert!(played.connected && played.video_frames >= 40, "{played:?}");
    assert_eq!(played.first_keyframe_at, Some(0), "the first picture a cold viewer is shown is a keyframe: {played:?}");

    drop(oxrtc);
    remove_dir(&dir).await;
}
