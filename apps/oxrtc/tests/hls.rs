//! HLS on the running server, the real path (M5 F6): go2rtc and FFmpeg as a
//! camera that sends AAC beside its H.264, oxrtc in process, and FFmpeg's own
//! HLS reader on the URLs a user gives VLC, ffplay or Home Assistant. Both
//! forms (MPEG-TS, and fMP4 with `&mp4`) carry the picture and the sound:
//! ffprobe reads `h264` and `aac` at the camera's rate, `ffmpeg -xerror`
//! decodes both with no error, and a segment alone holds both tracks for its
//! `EXTINF`. The file's `hls:` bounds hold in the running server. Skipped
//! (and passed) when FFmpeg or go2rtc is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::process::Stdio;

use common::http::Request;
use common::*;
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::Value;
use tokio::process::Command;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const ANY: &str = "127.0.0.1:0";

fn config(tools: &Tools, camera: &FakeCamera, hls: &str) -> String {
    format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{}\"\n{hls}\
         streams:\n  aac: \"{}\"\n",
        tools.ffmpeg.display(),
        camera.url("with_sound"),
    )
}

async fn get(api: SocketAddr, target: &str) -> common::http::Reply {
    Request::new("GET", target).auth(USER, PASS).send(api).await
}

/// What ffprobe reads of `url`: each stream's codec, rate and channels, and
/// how many frames of each it counted (with `count`).
async fn streams(tools: &Tools, url: &str, count: bool) -> Vec<Value> {
    let mut command = Command::new(&tools.ffprobe);
    command.args(["-v", "error"]);
    if count {
        command.arg("-count_frames");
    }
    let output = command
        .args(["-show_entries", "stream=codec_name,sample_rate,channels,nb_read_frames,duration:format=duration"])
        .args(["-of", "json", url])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "ffprobe {url}: {}", String::from_utf8_lossy(&output.stderr));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut streams = json["streams"].as_array().unwrap().clone();
    streams.sort_by_key(|s| s["codec_name"].as_str().unwrap_or_default().to_owned());
    streams
}

fn codecs(streams: &[Value]) -> Vec<&str> {
    streams.iter().map(|s| s["codec_name"].as_str().unwrap()).collect()
}

/// The complete segments a media playlist lists, oldest first: each
/// `EXTINF`, URI and init segment's URI (fMP4).
fn segments(playlist: &str) -> Vec<(f64, String, Option<String>)> {
    let lines: Vec<&str> = playlist.lines().collect();
    let mut map = None;
    let mut out = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if let Some(uri) = line.strip_prefix("#EXT-X-MAP:URI=\"") {
            map = Some(uri.trim_end_matches('"').to_owned());
        }
        if let Some(extinf) = line.strip_prefix("#EXTINF:") {
            let extinf = extinf.trim_end_matches(',').parse::<f64>().unwrap();
            out.push((extinf, lines[at + 1].to_owned(), map.clone()));
        }
    }
    out
}

/// The sound's packets of a file: presentation time and duration, seconds.
async fn sound_packets(tools: &Tools, path: &str) -> Vec<(f64, f64)> {
    let output = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "packet=pts_time,duration_time"])
        .args(["-of", "csv=p=0", path])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.lines()
        .filter_map(|l| l.split_once(','))
        .map(|(pts, d)| (pts.parse::<f64>().unwrap(), d.trim_end_matches(',').parse::<f64>().unwrap()))
        .collect()
}

#[tokio::test]
async fn ffmpeg_reads_both_hls_forms_of_a_camera_with_aac_sound() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start_with_aac(&tools).await;

    // The file's bounds hold in the running server: two sessions from this
    // address, the third refused as the limit says.
    let tight = config(&tools, &camera, "hls:\n  max_sessions_per_ip: 2\n  idle_seconds: 1\n");
    let app = App::start(&parse(&tight).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    for query in ["src=aac", "src=aac&mp4"] {
        let master = get(api, &format!("/api/stream.m3u8?{query}")).await;
        assert_eq!(master.status, 200, "{query}: {}", master.text());
        assert!(master.text().contains(",mp4a.40.2\""), "{query}: the sound is in CODECS: {}", master.text());
    }
    let third = get(api, "/api/stream.m3u8?src=aac").await;
    assert_eq!((third.status, third.text().as_str()), (503, "too many hls sessions"), "hls.max_sessions_per_ip: 2");
    assert!(app.shutdown().await);

    let app = App::start(&parse(&config(&tools, &camera, "")).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    let dir = temp_dir("hls-aac");
    for (form, query, extension) in [("MPEG-TS", "src=aac", "ts"), ("fMP4", "src=aac&mp4", "mp4")] {
        let url = format!("http://{USER}:{PASS}@{api}/api/stream.m3u8?{query}");
        let seen = streams(&tools, &url, false).await;
        assert_eq!(codecs(&seen), ["aac", "h264"], "{form}: {seen:?}");
        assert_eq!((seen[0]["sample_rate"].as_str(), seen[0]["channels"].as_u64()), (Some("16000"), Some(1)), "{form}");

        let output = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-v", "error", "-xerror", "-t", "6", "-i", &url, "-map", "0", "-f", "null", "-"])
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        let errors = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        assert!(output.status.success() && errors.is_empty(), "{form}: FFmpeg decodes picture and sound: {errors}");

        // Two segments alone, one after the other: both tracks in each, the
        // pictures its EXTINF says, the sound of one following the other's
        // without a gap or a repeat (the camera's FFmpeg sends five AAC frames
        // a packet, so the sound a segment holds is not cut at its picture's
        // times: only the video's keyframes cut).
        let master = get(api, &format!("/api/stream.m3u8?{query}")).await.text();
        let media = master.lines().last().unwrap().to_owned();
        let mut listed = Vec::new();
        for _ in 0..40 {
            listed = segments(&get(api, &format!("/api/{media}")).await.text());
            if listed.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(listed.len() >= 2, "{form}: two complete segments: {listed:?}");
        let mut sound = Vec::new();
        let mut extinfs = 0.0;
        for (index, (extinf, uri, map)) in listed[listed.len() - 2..].iter().enumerate() {
            let mut bytes = match map {
                Some(init) => get(api, &format!("/api/hls/{init}")).await.body,
                None => Vec::new(),
            };
            let segment = get(api, &format!("/api/hls/{uri}")).await;
            assert_eq!(segment.status, 200, "{form}: {uri}");
            bytes.extend_from_slice(&segment.body);
            let path = dir.join(format!("{index}.{extension}"));
            std::fs::write(&path, &bytes).unwrap();
            let path = path.to_str().unwrap();
            let alone = streams(&tools, path, true).await;
            assert_eq!(codecs(&alone), ["aac", "h264"], "{form}: a segment holds both tracks: {alone:?}");
            let pictures = alone[1]["nb_read_frames"].as_str().unwrap().parse::<f64>().unwrap();
            assert!((pictures - extinf * 25.0).abs() <= 1.0, "{form}: {pictures} pictures for an EXTINF of {extinf}");
            sound.extend(sound_packets(&tools, path).await);
            extinfs += extinf;
        }
        // AAC frames of 1024 samples at 16 kHz: 64 ms each, end to end.
        assert!(sound.iter().all(|(_, d)| (d - 0.064).abs() < 0.001), "{form}: {sound:?}");
        assert!(
            sound.windows(2).all(|w| (w[1].0 - w[0].0 - 0.064).abs() < 0.002),
            "{form}: no gap, no repeat: {sound:?}"
        );
        let held = sound.len() as f64 * 0.064;
        assert!((held - extinfs).abs() <= 0.4, "{form}: {held} s of sound in {extinfs} s of segments");
    }
    remove_dir(&dir).await;
    camera.stop().await;
    assert!(app.shutdown().await);
}

/// FFmpeg's DASH reader on `stream.mpd` of a camera's picture: each segment
/// read once, in order.
/// Its `$Number$` on a timeline is an index into the timeline it holds, so
/// once oxrtc's window had slid it went back for segments it had read at each
/// manifest reload (hundreds of "non monotonically increasing dts" in 10 s,
/// F6's report); `$Time$` it maps across reloads.
#[tokio::test]
async fn ffmpeg_reads_each_dash_segment_once_in_order() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start(&tools).await;
    let text = config(&tools, &camera, "").replace(&camera.url("with_sound"), &camera.url("h264"));
    let app = App::start(&parse(&text).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    let url = format!("http://{USER}:{PASS}@{api}/api/stream.mpd?src=aac");
    let output = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-v", "verbose", "-t", "20", "-i", &url, "-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let log = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{log}");
    let asked: Vec<&str> = log
        .lines()
        .filter_map(|l| l.split("DASH request for url '").nth(1))
        .filter(|u| u.contains("segment.m4s"))
        // Its start (`$Time$`), or its number as it was before F6P.
        .filter_map(|u| u.split("&t=").nth(1).or_else(|| u.split("&n=").nth(1)))
        .filter_map(|t| t.split('\'').next())
        .collect();
    let times: Vec<u64> = asked.iter().map(|t| t.parse().unwrap()).collect();
    // The camera's 2 s groups: ten segments, past the window's seven, so that
    // it slid under the reader.
    assert!(times.len() >= 9, "it read the stream: {asked:?}");
    assert!(times.windows(2).all(|w| w[0] < w[1]), "each segment once, in order: {times:?}");
    let backwards: Vec<&str> =
        log.lines().filter(|l| l.contains("non monotonically") || l.contains("out of order")).collect();
    assert!(backwards.is_empty(), "{} times backwards: {:?}", backwards.len(), backwards.first());
    camera.stop().await;
    assert!(app.shutdown().await);
}
/// FFmpeg's DASH reader on `stream.mpd` of a camera with AAC sound: the
/// picture and the sound as two streams at their own rates, decoded with no
/// error and no time going back. One representation of both read the sound as
/// the picture's stream, on the picture's time base, and never reached `-t`.
#[tokio::test]
async fn ffmpeg_reads_the_picture_and_the_sound_of_dash() {
    let Some(tools) = Tools::find() else { return skip("FFmpeg or go2rtc not found") };
    let camera = FakeCamera::start_with_aac(&tools).await;
    let app = App::start(&parse(&config(&tools, &camera, "")).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    let seen = streams(&tools, &format!("http://{USER}:{PASS}@{api}/api/stream.mpd?src=aac"), false).await;
    assert_eq!(codecs(&seen), ["aac", "h264"], "{seen:?}");
    assert_eq!((seen[0]["sample_rate"].as_str(), seen[0]["channels"].as_u64()), (Some("16000"), Some(1)));
    // Another query: another DASH session, read from its start.
    let url = format!("http://{USER}:{PASS}@{api}/api/stream.mpd?src=aac&mp4");
    let mut command = Command::new(&tools.ffmpeg);
    command
        .args(["-hide_banner", "-v", "warning", "-xerror", "-t", "8", "-i", &url, "-map", "0", "-f", "null", "-"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .expect("FFmpeg reached -t 8 within a minute")
        .unwrap();
    let log = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{log}");
    let backwards: Vec<&str> =
        log.lines().filter(|l| l.contains("non monotonically") || l.contains("out of order")).collect();
    assert!(backwards.is_empty(), "{} times backwards: {:?}", backwards.len(), backwards.first());
    camera.stop().await;
    assert!(app.shutdown().await);
}
