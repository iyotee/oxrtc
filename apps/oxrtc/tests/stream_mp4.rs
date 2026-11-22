//! `GET /api/stream.mp4` and `GET /api/frame.mp4` on the running server, the
//! real path (F4): go2rtc and FFmpeg as the camera, oxrtc in process, and
//! FFmpeg's own HTTP client reading the URLs a user gives VLC, ffplay or a
//! Home Assistant service. `stream.mp4?duration=5` holds the codecs the query
//! chose, about five seconds of pictures from a keyframe, timestamps that
//! rise, and decodes with no error; `frame.mp4` decodes to one picture.
//! Skipped (and passed) when FFmpeg or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::*;
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::Value;
use tokio::process::Command;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";

fn url(api: SocketAddr, path: &str) -> String {
    format!("http://{USER}:{PASS}@{api}{path}")
}

/// What ffprobe reads of `url`: each stream's codec, and each packet's
/// stream, presentation time (seconds) and whether it is a keyframe.
async fn probe_url(tools: &Tools, url: &str) -> (Vec<String>, Vec<(usize, f64, bool)>) {
    let output = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "stream=index,codec_name:packet=stream_index,pts_time,flags"])
        .args(["-of", "json", url])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "ffprobe: {}", String::from_utf8_lossy(&output.stderr));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let codecs = json["streams"].as_array().unwrap().iter().map(|s| s["codec_name"].as_str().unwrap().to_owned());
    let packets = json["packets"].as_array().unwrap().iter().map(|p| {
        let time = p["pts_time"].as_str().unwrap().parse::<f64>().unwrap();
        (p["stream_index"].as_u64().unwrap() as usize, time, p["flags"].as_str().unwrap().starts_with('K'))
    });
    (codecs.collect(), packets.collect())
}

/// `ffmpeg -xerror` decodes `url` whole: its error output, empty when none,
/// and the pictures it decoded.
async fn decode(tools: &Tools, url: &str) -> (String, usize) {
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-v", "error", "-xerror", "-i", url, "-map", "0:v", "-f", "framemd5", "-"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let pictures = String::from_utf8_lossy(&output.stdout).lines().filter(|l| !l.starts_with('#')).count();
    (if output.status.success() { errors } else { format!("exit {:?}: {errors}", output.status.code()) }, pictures)
}

/// The pictures of stream 0, checked: from a keyframe, rising.
fn pictures(packets: &[(usize, f64, bool)]) -> Vec<f64> {
    let times: Vec<f64> = packets.iter().filter(|p| p.0 == 0).map(|p| p.1).collect();
    let first = packets.iter().find(|p| p.0 == 0).unwrap();
    assert!(first.2 && first.1 == 0.0, "the first picture is a keyframe at zero: {first:?}");
    assert!(times.windows(2).all(|w| w[0] < w[1]), "presentation times rise: {times:?}");
    times
}

#[tokio::test]
async fn ffmpeg_reads_stream_mp4_and_frame_mp4() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let text = format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{}\"\n\
         streams:\n  sound: \"{}\"\n  hevc: \"{}\"\n",
        tools.ffmpeg.display(),
        camera.url("with_sound"),
        camera.url("h265"),
    );
    let app = App::start(&parse(&text).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;

    // H.264 at 25 pictures a second with A-law, which `mp4=flac` takes.
    let five = url(api, "/api/stream.mp4?src=sound&mp4=flac&duration=5");
    let began = Instant::now();
    let (codecs, packets) = probe_url(&tools, &five).await;
    let took = began.elapsed();
    assert_eq!(codecs, ["h264", "flac"], "G.711 as FLAC");
    let times = pictures(&packets);
    assert!((115..=126).contains(&times.len()), "five seconds of 25 a second: {}", times.len());
    assert!(*times.last().unwrap() < 5.0, "nothing at or past 5 s: {times:?}");
    assert!(packets.iter().filter(|p| p.0 == 1).count() >= 60, "the sound beside it");
    assert!(took < Duration::from_secs(12), "the response ended: {took:?}");
    let (errors, decoded) = decode(&tools, &five).await;
    assert_eq!(errors, "", "FFmpeg decodes it with no error");
    assert!(decoded >= 115, "{decoded} pictures decoded");

    // Without `mp4`: go2rtc's default, H.264 or H.265 and AAC; G.711 is not
    // in it.
    let (codecs, _) = probe_url(&tools, &url(api, "/api/stream.mp4?src=sound&duration=1")).await;
    assert_eq!(codecs, ["h264"]);
    let (codecs, packets) = probe_url(&tools, &url(api, "/api/stream.mp4?src=hevc&duration=2")).await;
    assert_eq!(codecs, ["hevc"], "the camera's own H.265");
    pictures(&packets);

    // One keyframe, one picture.
    for src in ["sound", "hevc"] {
        let frame = url(api, &format!("/api/frame.mp4?src={src}"));
        let (codecs, packets) = probe_url(&tools, &frame).await;
        assert_eq!((codecs.len(), packets.len()), (1, 1), "{src}: the picture alone, one packet");
        assert!(packets[0].2, "{src}: a keyframe");
        assert_eq!(decode(&tools, &frame).await, (String::new(), 1), "{src}: decodes to one picture");
    }

    camera.stop().await;
    assert!(app.shutdown().await);
}
