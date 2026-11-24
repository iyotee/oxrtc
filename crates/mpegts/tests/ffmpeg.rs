//! The writer's streams read by real FFmpeg: real encoded input (the test
//! pattern and a sine through libx264, libx265, FFmpeg's AAC and libopus, made
//! by the MP4 writer's helpers, `crates/mp4/tests/common`) written as
//! MPEG-TS with the tables at the start of every second, as HLS segments
//! are; ffprobe reads back the codecs, one packet per frame at its time,
//! and ffmpeg decodes every frame with no error. Skipped when the binaries
//! are absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

#[path = "../../mp4/tests/common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use common::tools::{ffmpeg, ffprobe, run};
use common::{Media, interleave};
use oxrtc_core::{Codec, Frame, TrackInfo};
use oxrtc_mpegts::{TIME_OFFSET, Writer};

fn fields(line: &str) -> HashMap<String, String> {
    line.split('|').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

/// `media` as one MPEG-TS stream, the tables before the first frame and
/// then before every video keyframe at least a second after the last ones.
fn write(media: &[&Media]) -> Vec<u8> {
    let tracks: Vec<TrackInfo> = media.iter().map(|m| m.info.clone()).collect();
    let mut writer = Writer::new(&tracks).unwrap();
    let mut out = Vec::new();
    let mut last_tables: Option<i64> = None;
    for f in interleave(media) {
        let rate = i64::from(tracks[f.track].clock_rate);
        let video = tracks[f.track].codec.kind() == oxrtc_core::MediaKind::Video;
        let due = last_tables.is_none_or(|at| f.pts * 90_000 / rate - at >= 90_000);
        if last_tables.is_none() || (video && f.key && due) {
            out.extend_from_slice(&writer.tables());
            last_tables = Some(f.pts * 90_000 / rate);
        }
        let frame = Frame::new(f.track as u8, f.pts, f.key, f.data.clone());
        out.extend_from_slice(&writer.push_decoded(f.track, &frame, f.dts).unwrap());
    }
    out
}

async fn check(dir: &Path, name: &str, media: &[&Media], codecs: &[&str]) {
    let path = dir.join(format!("{name}.ts"));
    std::fs::write(&path, write(media)).unwrap();
    let probe = ffprobe().unwrap();
    let entries = "stream=index,codec_name:packet=stream_index,pts,flags";
    let p = path.to_str().unwrap();
    let (ok, out, err) =
        run(&probe, &["-v", "error", "-show_entries", entries, "-of", "compact=p=1", p], Duration::from_secs(60)).await;
    assert!(ok && err.trim().is_empty(), "{name}: ffprobe: {err}");
    let streams: Vec<_> = out.lines().filter_map(|l| l.strip_prefix("stream|")).map(fields).collect();
    let packets: Vec<_> = out.lines().filter_map(|l| l.strip_prefix("packet|")).map(fields).collect();
    let names: Vec<&str> = streams.iter().map(|s| s["codec_name"].as_str()).collect();
    assert_eq!(names, codecs, "{name}");
    for (index, m) in media.iter().enumerate() {
        let of: Vec<_> = packets.iter().filter(|p| p["stream_index"] == index.to_string()).collect();
        assert_eq!(of.len(), m.frames.len(), "{name}: packets of stream {index}");
        let rate = i64::from(m.info.clock_rate);
        for (packet, frame) in of.iter().zip(&m.frames) {
            let expected = frame.pts * 90_000 / rate + TIME_OFFSET as i64;
            assert_eq!(packet["pts"].parse::<i64>().unwrap(), expected, "{name}: stream {index}");
            if m.info.codec.kind() == oxrtc_core::MediaKind::Video {
                assert_eq!(packet["flags"].starts_with('K'), frame.key, "{name}: a keyframe flag");
            }
        }
    }
    let ff = ffmpeg().unwrap();
    let args = ["-v", "error", "-xerror", "-i", p, "-map", "0", "-f", "null", "-"];
    let (ok, _, err) = run(&ff, &args, Duration::from_secs(60)).await;
    assert!(ok && err.trim().is_empty(), "{name}: decoding failed: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_h264_with_aac_and_h265_with_opus() {
    let Some(ff) = ffmpeg() else {
        eprintln!("ffmpeg not found: skipped");
        return;
    };
    if ffprobe().is_none() {
        eprintln!("ffprobe not found: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let h264 = common::video(&ff, Codec::H264, 4).await;
    let aac = common::aac(&ff, 4, 1).await;
    check(dir.path(), "h264_aac", &[&h264, &aac], &["h264", "aac"]).await;
    let h265 = common::video(&ff, Codec::H265, 4).await;
    let opus = common::opus(&ff, 4, 1).await;
    check(dir.path(), "h265_opus", &[&h265, &opus], &["hevc", "opus"]).await;
    check(dir.path(), "aac_alone", &[&common::aac(&ff, 2, 0).await], &["aac"]).await;
}
