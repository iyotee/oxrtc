//! The writer's files read by real FFmpeg: real encoded input (test pattern
//! and sine through libx264, libx265, FFmpeg's AAC, libopus, G.711 A-law)
//! written as an init segment and fragments, then ffprobe reads back the
//! codecs, the frame counts and the timestamps, and ffmpeg decodes every
//! frame with no error. Skipped when the binaries are absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::tools::{ffmpeg, ffprobe, run};
use common::{Media, START_SECONDS, Timed, file, write};
use oxrtc_core::Codec;
use oxrtc_mp4::{Fragmenting, Options};

/// One `key=value|key=value` line of ffprobe's compact output.
fn fields(line: &str) -> HashMap<String, String> {
    line.split('|').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

/// ffprobe's streams and packets of `path`.
async fn probe(path: &Path) -> (Vec<HashMap<String, String>>, Vec<HashMap<String, String>>) {
    let ffprobe = ffprobe().unwrap();
    let path = path.to_str().unwrap();
    let entries = "stream=index,codec_name,width,height,sample_rate,channels:packet=stream_index,pts,dts,flags";
    let args = ["-v", "error", "-show_entries", entries, "-of", "compact=p=1", path];
    let (ok, out, err) = run(&ffprobe, &args, Duration::from_secs(60)).await;
    assert!(ok && err.trim().is_empty(), "ffprobe: {err}");
    let pick = |section: &str| -> Vec<HashMap<String, String>> {
        out.lines().filter_map(|l| l.strip_prefix(section)).map(fields).collect()
    };
    (pick("stream|"), pick("packet|"))
}

/// Decodes every stream of `path` to nothing: FFmpeg must say nothing.
async fn decodes_cleanly(path: &Path) {
    let ffmpeg = ffmpeg().unwrap();
    let args = ["-v", "error", "-xerror", "-i", path.to_str().unwrap(), "-map", "0", "-f", "null", "-"];
    let (ok, _, err) = run(&ffmpeg, &args, Duration::from_secs(60)).await;
    assert!(ok && err.trim().is_empty(), "decoding failed: {err}");
}

/// Writes `media` with `options` to a file and checks what ffprobe reads:
/// the codecs, one packet per frame, the decode times of the source,
/// rising, and the presentation times of the source.
async fn check(dir: &Path, name: &str, media: &[&Media], options: Options, codecs: &[&str]) -> PathBuf {
    let (writer, fragments) = write(media, options);
    let path = dir.join(format!("{name}.mp4"));
    std::fs::write(&path, file(&writer, &fragments)).unwrap();
    let (streams, packets) = probe(&path).await;
    let names: Vec<&str> = streams.iter().map(|s| s["codec_name"].as_str()).collect();
    assert_eq!(names, codecs, "{name}");
    for (index, m) in media.iter().enumerate() {
        let of: Vec<&HashMap<String, String>> =
            packets.iter().filter(|p| p["stream_index"] == index.to_string()).collect();
        assert_eq!(of.len(), m.frames.len(), "{name}: packets of stream {index}");
        let dts: Vec<i64> = of.iter().map(|p| p["dts"].parse().unwrap()).collect();
        let pts: Vec<i64> = of.iter().map(|p| p["pts"].parse().unwrap()).collect();
        assert!(dts.windows(2).all(|w| w[0] < w[1]), "{name}: stream {index}'s decode times rise");
        // A steady source is placed on its own times, so ffprobe reads them back.
        let given: Vec<i64> = m.frames.iter().map(|f| f.dts).collect();
        assert_eq!(dts, given, "{name}: stream {index}'s decode times");
        let mut shown: Vec<i64> = m.frames.iter().map(|f| f.pts).collect();
        let mut read = pts.clone();
        shown.sort_unstable();
        read.sort_unstable();
        assert_eq!(read, shown, "{name}: stream {index}'s presentation times");
        let keys = of.iter().filter(|p| p["flags"].starts_with('K')).count();
        assert_eq!(keys, m.frames.iter().filter(|f| f.key).count(), "{name}: keyframes of stream {index}");
    }
    decodes_cleanly(&path).await;
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_and_aac_one_fragment_per_frame() {
    let Some(ffmpeg) = ffmpeg() else { return };
    if ffprobe().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = common::video(&ffmpeg, Codec::H264, 4).await;
    let sound = common::aac(&ffmpeg, 4, 1).await;
    assert_eq!(video.frames.len(), 100);
    assert_eq!(video.frames[0].dts, START_SECONDS * 90_000, "the timeline does not start at 0");
    assert!(sound.frames.len() >= 185, "{} AAC frames", sound.frames.len());
    check(dir.path(), "h264_aac", &[&video, &sound], Options::default(), &["h264", "aac"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn h265_and_opus_one_fragment_per_group_of_pictures() {
    let Some(ffmpeg) = ffmpeg() else { return };
    if ffprobe().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = common::video(&ffmpeg, Codec::H265, 4).await;
    let sound = common::opus(&ffmpeg, 4, 1).await;
    // 4 s of 20 ms packets, and one for libopus's look-ahead.
    assert!((200..=201).contains(&sound.frames.len()), "{} Opus packets", sound.frames.len());
    let options = Options { fragmenting: Fragmenting::GroupOfPictures, ..Options::default() };
    let (_, fragments) = write(&[&video, &sound], options);
    assert_eq!(fragments.len(), 4, "a group of 25 pictures per second");
    assert!(fragments.iter().all(|f| f.independent));
    check(dir.path(), "h265_opus", &[&video, &sound], options, &["hevc", "opus"]).await;
}

/// G.711 A-law packed as FLAC, in parts of half a second: FFmpeg's FLAC
/// decoder gives back exactly the samples its own A-law decoder gives.
#[tokio::test(flavor = "multi_thread")]
async fn pcma_as_flac_decodes_to_the_very_samples() {
    let Some(ffmpeg) = ffmpeg() else { return };
    if ffprobe().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = common::video(&ffmpeg, Codec::H264, 4).await;
    let (sound, alaw) = common::pcma(&ffmpeg, 4, 1).await;
    assert_eq!(sound.frames.len(), 200);
    let options = Options { fragmenting: Fragmenting::Duration(Duration::from_millis(500)), ..Options::default() };
    let path = check(dir.path(), "h264_pcma", &[&video, &sound], options, &["h264", "flac"]).await;

    let alaw_in = dir.path().join("in.alaw");
    std::fs::write(&alaw_in, &alaw).unwrap();
    let (theirs, ours) = (dir.path().join("theirs.pcm"), dir.path().join("ours.pcm"));
    let (alaw_s, path_s) = (alaw_in.to_str().unwrap(), path.to_str().unwrap());
    let (theirs_s, ours_s) = (theirs.to_str().unwrap(), ours.to_str().unwrap());
    let from_alaw =
        ["-v", "error", "-f", "alaw", "-ar", "8000", "-ac", "1", "-i", alaw_s, "-f", "s16le", "-y", theirs_s];
    let from_mp4 = ["-v", "error", "-i", path_s, "-map", "0:a", "-f", "s16le", "-y", ours_s];
    for args in [&from_alaw[..], &from_mp4[..]] {
        let (ok, _, err) = run(&ffmpeg, args, Duration::from_secs(60)).await;
        assert!(ok, "{err}");
    }
    let (theirs, ours) = (std::fs::read(&theirs).unwrap(), std::fs::read(&ours).unwrap());
    assert_eq!(theirs.len(), 4 * 8000 * 2);
    assert!(ours == theirs, "the FLAC track decodes to other samples than the A-law it was made from");
}

/// B-frames: libx264 with two of them, the decode and presentation times its
/// MPEG-TS gives each packet; the composition offsets carry the order.
#[tokio::test(flavor = "multi_thread")]
async fn b_frames_keep_their_presentation_times() {
    let Some(ffmpeg) = ffmpeg() else { return };
    let Some(ffprobe) = ffprobe() else { return };
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("b.ts");
    let h264 = dir.path().join("b.h264");
    let (ts_s, h264_s) = (ts.to_str().unwrap(), h264.to_str().unwrap());
    let encode = [
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=25",
        "-t",
        "4",
        "-c:v",
        "libx264",
        "-bf",
        "2",
        "-g",
        "25",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "mpegts",
        "-y",
        ts_s,
    ];
    assert!(run(&ffmpeg, &encode, Duration::from_secs(60)).await.0);
    let copy =
        ["-v", "error", "-i", ts_s, "-c", "copy", "-bsf:v", "h264_metadata=aud=insert", "-f", "h264", "-y", h264_s];
    assert!(run(&ffmpeg, &copy, Duration::from_secs(60)).await.0);
    let probe = ["-v", "error", "-show_entries", "packet=pts,dts", "-of", "compact=p=0", ts_s];
    let (ok, out, _) = run(&ffprobe, &probe, Duration::from_secs(60)).await;
    assert!(ok);
    let times: Vec<(i64, i64)> =
        out.lines().map(fields).map(|f| (f["pts"].parse().unwrap(), f["dts"].parse().unwrap())).collect();
    let (params, units) = common::access_units(Codec::H264, &std::fs::read(&h264).unwrap());
    assert_eq!(units.len(), times.len());
    assert!(times.iter().any(|(pts, dts)| pts != dts), "the encode has no B-frame");
    let frames: Vec<Timed> = units
        .into_iter()
        .zip(&times)
        .map(|((key, data), &(pts, dts))| Timed { track: 0, pts, dts, key, data })
        .collect();
    let video = Media { info: oxrtc_core::TrackInfo::video(Codec::H264).with_params(params), frames };
    check(dir.path(), "h264_b_frames", &[&video], Options::default(), &["h264"]).await;
}

/// The files `tests/browser/mse.mjs` feeds to MSE in a real browser (run by
/// hand, see the crate's README): six seconds of H.264 with AAC, with Opus
/// and with A-law packed as FLAC, one fragment per frame, and for each a JSON
/// file with the MIME type and every fragment's size and time. A fourth case
/// is a camera's uneven clock: the picture's steps 32 to 48 ms, and 200 ms
/// missing at 3 s (a source that stalled), which must leave no gap in what
/// the browser buffers. H.265 with AAC, as `hvc1` and as `hev1`, is written
/// too and marked optional: a browser with no HEVC decoder says so through
/// `isTypeSupported`, and the script reports it without failing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes the browser proof's files: OXRTC_MP4_BROWSER_DIR=<dir>"]
async fn browser_files() {
    let dir = PathBuf::from(std::env::var("OXRTC_MP4_BROWSER_DIR").expect("OXRTC_MP4_BROWSER_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let ffmpeg = ffmpeg().expect("ffmpeg");
    let hevc = common::video(&ffmpeg, Codec::H265, 6).await;
    let video = common::video(&ffmpeg, Codec::H264, 6).await;
    let aac = common::aac(&ffmpeg, 6, 1).await;
    let opus = common::opus(&ffmpeg, 6, 1).await;
    let (pcma, _) = common::pcma(&ffmpeg, 6, 1).await;
    let mut uneven = Media { info: video.info.clone(), frames: video.frames.clone() };
    let mut shift = 0;
    for (i, f) in uneven.frames.iter_mut().enumerate() {
        if i == 75 {
            shift += 18_000;
        }
        let jitter = [0, 720, -720, 1440, -1440][i % 5];
        f.pts += shift + jitter;
        f.dts = f.pts;
    }
    let hev1 = Options { hevc: oxrtc_mp4::HevcEntry::Hev1, ..Options::default() };
    let cases = [
        ("h264_aac", &video, &aac, Options::default(), false),
        ("h264_opus", &video, &opus, Options::default(), false),
        ("h264_pcma", &video, &pcma, Options::default(), false),
        ("h264_aac_uneven", &uneven, &aac, Options::default(), false),
        ("h265_aac", &hevc, &aac, Options::default(), true),
        ("h265_aac_hev1", &hevc, &aac, hev1, true),
    ];
    for (name, video, sound, options, optional) in cases {
        let (writer, fragments) = write(&[video, sound], options);
        std::fs::write(dir.join(format!("{name}.init.mp4")), writer.init_segment()).unwrap();
        let body: Vec<u8> = fragments.iter().flat_map(|f| f.bytes.iter().copied()).collect();
        std::fs::write(dir.join(format!("{name}.m4s")), body).unwrap();
        let list: Vec<String> = fragments
            .iter()
            .map(|f| format!("{{\"size\":{},\"time\":{:.6}}}", f.bytes.len(), f.span.start().as_secs_f64()))
            .collect();
        let json = format!(
            "{{\"name\":\"{name}\",\"optional\":{optional},\"mime\":\"{}\",\"fragments\":[{}]}}",
            writer.mime_type().replace('"', "\\\""),
            list.join(",")
        );
        std::fs::write(dir.join(format!("{name}.json")), json).unwrap();
    }
}
