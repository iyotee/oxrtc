//! Real encoded input for the writer, made by FFmpeg (test pattern and sine),
//! through the RTSP server's helpers for running it (`tools`): video access
//! units with their parameter sets, AAC from ADTS, Opus from Ogg, G.711
//! A-law in 20 ms frames; then interleaved as a subscription delivers them
//! and written.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

#[path = "../../../rtsp-server/tests/common/tools.rs"]
pub mod tools;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::frame::{
    AnnexBUnits, annex_b_to_avcc, h264_is_keyframe, h264_nal_type, h265_is_keyframe, h265_nal_type,
};
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};
use oxrtc_mp4::{Fragment, Options, Writer};

/// Where every track's timeline starts: 10 s, not 0, as on a hub that has run.
pub const START_SECONDS: i64 = 10;

/// One frame of a track, with its decode time.
#[derive(Clone, Debug)]
pub struct Timed {
    pub track: usize,
    pub pts: i64,
    pub dts: i64,
    pub key: bool,
    pub data: Bytes,
}

/// A track and its frames.
pub struct Media {
    pub info: TrackInfo,
    pub frames: Vec<Timed>,
}

/// Splits an Annex B stream with access unit delimiters into access units;
/// the parameter sets go to the track's `CodecParams`, as a depacketizer puts them.
pub fn access_units(codec: Codec, data: &[u8]) -> (CodecParams, Vec<(bool, Bytes)>) {
    let mut sets: Vec<(u8, Bytes)> = Vec::new();
    let mut units: Vec<Vec<u8>> = Vec::new();
    for nal in AnnexBUnits::new(data) {
        let (kind, aud, set) = match codec {
            Codec::H264 => {
                let t = h264_nal_type(nal).unwrap();
                (t, t == 9, matches!(t, 7 | 8))
            }
            _ => {
                let t = h265_nal_type(nal).unwrap();
                (t, t == 35, matches!(t, 32..=34))
            }
        };
        if aud {
            units.push(Vec::new());
        } else if set {
            if !sets.iter().any(|(k, _)| *k == kind) {
                sets.push((kind, Bytes::copy_from_slice(nal)));
            }
        } else if let Some(unit) = units.last_mut() {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
    }
    let set = |kind: u8| sets.iter().find(|(k, _)| *k == kind).unwrap().1.clone();
    let params = match codec {
        Codec::H264 => CodecParams::H264 { sps: set(7), pps: set(8) },
        _ => CodecParams::H265 { vps: set(32), sps: set(33), pps: set(34) },
    };
    let frames = units
        .into_iter()
        .filter(|u| !u.is_empty())
        .map(|u| {
            let avcc = annex_b_to_avcc(&u).unwrap();
            let key = if codec == Codec::H264 { h264_is_keyframe(&avcc) } else { h265_is_keyframe(&avcc) };
            (key, avcc)
        })
        .collect();
    (params, frames)
}

/// `seconds` of 25 fps test pattern, no B-frames (`tools::encode`).
pub async fn video(ffmpeg: &Path, codec: Codec, seconds: u32) -> Media {
    let data = tools::encode(ffmpeg, codec, seconds).await;
    let (params, units) = access_units(codec, &data);
    let start = START_SECONDS * 90_000;
    let frames = units
        .into_iter()
        .enumerate()
        .map(|(i, (key, data))| {
            let pts = start + i as i64 * 3600;
            Timed { track: 0, pts, dts: pts, key, data }
        })
        .collect();
    Media { info: TrackInfo::video(codec).with_params(params), frames }
}

/// Runs FFmpeg on `args` and returns the file it wrote, `out` in a new directory.
async fn ffmpeg_file(ffmpeg: &Path, args: &[&str], out: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(out);
    let path_s = path.to_str().unwrap().to_owned();
    let mut all = vec!["-hide_banner", "-loglevel", "error"];
    all.extend_from_slice(args);
    all.extend_from_slice(&["-y", &path_s]);
    let (ok, _, err) = tools::run(ffmpeg, &all, Duration::from_secs(60)).await;
    assert!(ok, "ffmpeg {args:?}: {err}");
    tokio::fs::read(&path).await.unwrap()
}

fn sine(seconds: u32, rate: u32) -> String {
    format!("sine=frequency=440:sample_rate={rate}:duration={seconds}")
}

/// AAC-LC at 48 kHz, stereo, from ADTS: each frame's header dropped, the
/// first one's fields made into the `AudioSpecificConfig`.
pub async fn aac(ffmpeg: &Path, seconds: u32, track: usize) -> Media {
    let source = sine(seconds, 48_000);
    let args = ["-f", "lavfi", "-i", &source, "-ac", "2", "-c:a", "aac", "-b:a", "96k", "-f", "adts"];
    let data = ffmpeg_file(ffmpeg, &args, "sound.aac").await;
    let mut at = 0;
    let mut config = None;
    let mut frames = Vec::new();
    while at + 7 <= data.len() {
        let h = &data[at..at + 7];
        assert_eq!((h[0], h[1] & 0xf0), (0xff, 0xf0), "ADTS sync at {at}");
        let header = if h[1] & 1 == 1 { 7 } else { 9 };
        let length = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        let (profile, rate, channels) = ((h[2] >> 6) + 1, (h[2] >> 2) & 0xf, ((h[2] & 1) << 2) | (h[3] >> 6));
        config.get_or_insert_with(|| {
            Bytes::from(vec![(profile << 3) | (rate >> 1), ((rate & 1) << 7) | (channels << 3)])
        });
        let pts = START_SECONDS * 48_000 + frames.len() as i64 * 1024;
        frames.push(Timed {
            track,
            pts,
            dts: pts,
            key: true,
            data: Bytes::copy_from_slice(&data[at + header..at + length]),
        });
        at += length;
    }
    let config = config.unwrap();
    Media { info: TrackInfo::audio(Codec::Aac, 48_000, 2).with_params(CodecParams::Aac { config }), frames }
}

/// Opus at 48 kHz, stereo, 20 ms packets, from Ogg: its two header packets dropped.
pub async fn opus(ffmpeg: &Path, seconds: u32, track: usize) -> Media {
    let source = sine(seconds, 48_000);
    let args = [
        "-f",
        "lavfi",
        "-i",
        &source,
        "-ac",
        "2",
        "-c:a",
        "libopus",
        "-b:a",
        "64k",
        "-frame_duration",
        "20",
        "-f",
        "ogg",
    ];
    let data = ffmpeg_file(ffmpeg, &args, "sound.ogg").await;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut open = false;
    let mut at = 0;
    while at + 27 <= data.len() {
        assert_eq!(&data[at..at + 4], b"OggS");
        let segments = usize::from(data[at + 26]);
        let table = &data[at + 27..at + 27 + segments];
        let mut body = at + 27 + segments;
        for &lace in table {
            if !open {
                packets.push(Vec::new());
            }
            packets.last_mut().unwrap().extend_from_slice(&data[body..body + usize::from(lace)]);
            body += usize::from(lace);
            open = lace == 255;
        }
        at = body;
    }
    assert!(packets[0].starts_with(b"OpusHead") && packets[1].starts_with(b"OpusTags"));
    let frames = packets[2..]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let pts = START_SECONDS * 48_000 + i as i64 * 960;
            Timed { track, pts, dts: pts, key: true, data: Bytes::copy_from_slice(p) }
        })
        .collect();
    Media { info: TrackInfo::audio(Codec::Opus, 48_000, 2), frames }
}

/// G.711 A-law at 8 kHz, mono, in 20 ms frames, and the raw A-law it was cut from.
pub async fn pcma(ffmpeg: &Path, seconds: u32, track: usize) -> (Media, Vec<u8>) {
    let source = sine(seconds, 8000);
    let args = ["-f", "lavfi", "-i", &source, "-ac", "1", "-c:a", "pcm_alaw", "-f", "alaw"];
    let data = ffmpeg_file(ffmpeg, &args, "sound.alaw").await;
    let frames = data
        .chunks(160)
        .enumerate()
        .map(|(i, c)| {
            let pts = START_SECONDS * 8000 + i as i64 * 160;
            Timed { track, pts, dts: pts, key: true, data: Bytes::copy_from_slice(c) }
        })
        .collect();
    (Media { info: TrackInfo::audio(Codec::Pcma, 8000, 1), frames }, data)
}

/// The frames of every track in the order a subscription delivers them: by
/// time, video first on a tie.
pub fn interleave(media: &[&Media]) -> Vec<Timed> {
    let mut all: Vec<(f64, usize, Timed)> = Vec::new();
    for m in media {
        for f in &m.frames {
            all.push((f.dts as f64 / f64::from(m.info.clock_rate), f.track, f.clone()));
        }
    }
    all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    all.into_iter().map(|(_, _, f)| f).collect()
}

/// A writer for `media`, fed every frame; its fragments, the last one flushed.
pub fn write(media: &[&Media], options: Options) -> (Writer, Vec<Fragment>) {
    let tracks: Vec<TrackInfo> = media.iter().map(|m| m.info.clone()).collect();
    let mut writer = Writer::new(&tracks, options).unwrap();
    let mut fragments = Vec::new();
    for f in interleave(media) {
        let frame = Frame::new(f.track as u8, f.pts, f.key, f.data.clone());
        fragments.extend(writer.push_decoded(f.track, &frame, f.dts).unwrap());
    }
    fragments.extend(writer.flush().unwrap());
    (writer, fragments)
}

/// The init segment and every fragment, as one file.
pub fn file(writer: &Writer, fragments: &[Fragment]) -> Vec<u8> {
    let mut out = writer.init_segment().to_vec();
    for f in fragments {
        out.extend_from_slice(&f.bytes);
    }
    out
}
