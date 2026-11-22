//! The harnesses of the two fuzz targets (`fuzz/`) and of their property
//! tests: any bytes into the test reader (`mp4_reader`), and tracks and frames
//! made from any bytes through the writer and read back (`mp4_writer`).
//! Each returns an error naming the invariant that broke; a panic is a
//! defect by itself.

use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, Frame, MediaKind, TrackInfo};

use crate::reader::{read_fragment, read_init};
use crate::{Fragment, Fragmenting, Options, Writer};

/// Parameter sets of libx264 and libx265 (FFmpeg 8.0.1): High 640x360, Main 1080p.
pub const H264_SPS: &str = "6764001eacd940a02ff97011000003000100000300320f162d96";
/// The PPS beside [`H264_SPS`].
pub const H264_PPS: &str = "68ebe3cb22c0";
/// The VPS beside [`H265_SPS`].
pub const H265_VPS: &str = "40010c01ffff016000000300900000030000030078959809";
/// Main, 1920x1080, level 4.
pub const H265_SPS: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";
/// The PPS beside [`H265_SPS`].
pub const H265_PPS: &str = "4401c172b46240";

/// Bytes from hexadecimal; a malformed pair is skipped.
pub fn hex(s: &str) -> Bytes {
    let pairs = s.as_bytes().chunks(2).filter_map(|p| std::str::from_utf8(p).ok());
    Bytes::from(pairs.filter_map(|p| u8::from_str_radix(p, 16).ok()).collect::<Vec<_>>())
}

/// Any bytes as an init segment and as a fragment: a value or an error, and
/// every sample's bytes within the input.
pub fn reader_bytes(data: &[u8]) -> Result<(), String> {
    let trex = read_init(data).map(|init| init.trex).unwrap_or_default();
    if let Ok(fragment) = read_fragment(data, &trex) {
        let total: usize = fragment.runs.iter().flat_map(|r| &r.samples).map(|s| s.data.len()).sum();
        if total > data.len() {
            return Err(format!("{total} bytes of samples read from {} bytes", data.len()));
        }
    }
    Ok(())
}

fn video_track(choice: u8, extra: &Bytes) -> Option<TrackInfo> {
    let fuzzed = |head: &[u8]| Bytes::from([head, extra].concat());
    let params = match choice % 5 {
        1 => CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) },
        2 => CodecParams::H265 { vps: hex(H265_VPS), sps: hex(H265_SPS), pps: hex(H265_PPS) },
        3 => CodecParams::H264 { sps: fuzzed(&[0x67]), pps: hex(H264_PPS) },
        4 => CodecParams::H265 { vps: hex(H265_VPS), sps: fuzzed(&[0x42, 0x01]), pps: hex(H265_PPS) },
        _ => return None,
    };
    let codec = if matches!(params, CodecParams::H264 { .. }) { Codec::H264 } else { Codec::H265 };
    Some(TrackInfo::video(codec).with_params(params))
}

fn audio_track(choice: u8, extra: &Bytes) -> Option<TrackInfo> {
    let aac = |config: Bytes| TrackInfo::audio(Codec::Aac, 48_000, 2).with_params(CodecParams::Aac { config });
    Some(match choice % 7 {
        1 => aac(Bytes::from_static(&[0x11, 0x90])),
        2 => TrackInfo::audio(Codec::Opus, 48_000, 2),
        3 => TrackInfo::audio(Codec::Pcma, 8000, 1),
        4 => TrackInfo::audio(Codec::Pcmu, 8000, 1),
        5 => TrackInfo::audio(Codec::L16, 16_000, 2),
        6 => aac(extra.clone()),
        _ => return None,
    })
}

/// One accepted frame, as the reader must give it back.
struct Expected {
    track: usize,
    data: Bytes,
    offset: i64,
}

/// Tracks, options and frames from `data`, through the writer, read back.
pub fn writer_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&[choice, extra_len], rest)) = data.split_first_chunk::<2>() else {
        return Ok(());
    };
    let (extra, mut ops) = rest.split_at_checked(usize::from(extra_len % 48)).unwrap_or((rest, &[]));
    let extra = Bytes::copy_from_slice(extra);
    let tracks: Vec<TrackInfo> =
        [video_track(choice & 7, &extra), audio_track((choice >> 3) & 7, &extra)].into_iter().flatten().collect();
    // The core's reading of an SPS's profile, tier and level (what a WebRTC
    // answer is negotiated on) is the sample entry's, for every SPS this
    // crate reads whole.
    for track in &tracks {
        if let CodecParams::H265 { sps, .. } = &track.params
            && let Ok(whole) = crate::h265::parse_sps(sps)
            && oxrtc_core::h265::sps_profile_tier_level(sps) != Some(whole.ptl)
        {
            return Err("the core reads another profile, tier and level than the sample entry".into());
        }
    }
    let fragmenting = match choice >> 6 {
        0 => Fragmenting::EveryFrame,
        1 => Fragmenting::GroupOfPictures,
        _ => Fragmenting::Duration(std::time::Duration::from_millis(u64::from(extra_len) * 4)),
    };
    let Ok(mut writer) = Writer::new(&tracks, Options { fragmenting, ..Options::default() }) else { return Ok(()) };
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut expected = Vec::new();
    let mut clocks = vec![1_000_000i64; tracks.len()];
    while let Some((&[op, len, step], tail)) = ops.split_first_chunk::<3>() {
        let (payload, after) = tail.split_at_checked(usize::from(len)).unwrap_or((tail, &[]));
        ops = after;
        let track = usize::from(op & 1) % tracks.len();
        let clock = clocks.get_mut(track).ok_or("clock")?;
        *clock += match (op >> 2) & 3 {
            0 => 3600,
            1 => 0,
            2 => 900_000,
            _ => i64::from(step) * 97,
        };
        let pts = *clock;
        let dts = if op & 0x10 != 0 { pts - i64::from(step) * 100 } else { pts };
        let nal_based = tracks.get(track).is_some_and(|t| t.codec.is_nal_based());
        let body = if nal_based && op & 0x40 == 0 {
            let mut unit = u32::try_from(payload.len() + 1).unwrap_or(0).to_be_bytes().to_vec();
            unit.push(if op & 2 != 0 { 0x65 } else { 0x41 });
            unit.extend_from_slice(payload);
            Bytes::from(unit)
        } else {
            Bytes::copy_from_slice(payload)
        };
        let frame = Frame::new(0, pts, op & 2 != 0, body.clone());
        match writer.push_decoded(track, &frame, dts) {
            Ok(closed) => {
                expected.push(Expected { track, data: body, offset: pts - dts });
                fragments.extend(closed);
            }
            Err(_) => continue,
        }
    }
    fragments.extend(writer.flush().map_err(|e| format!("flush: {e}"))?);
    check(&writer, &tracks, &fragments, &expected)
}

fn check(writer: &Writer, tracks: &[TrackInfo], fragments: &[Fragment], expected: &[Expected]) -> Result<(), String> {
    let init = read_init(writer.init_segment()).map_err(|e| format!("the init segment does not read: {e}"))?;
    if init.tracks.len() != tracks.len() {
        return Err(format!("{} tracks read back of {}", init.tracks.len(), tracks.len()));
    }
    let mut ends: Vec<Option<u64>> = vec![None; tracks.len()];
    let mut next: Vec<usize> = vec![0; tracks.len()];
    let of_track = |t: usize| expected.iter().filter(move |e| e.track == t);
    for (n, fragment) in fragments.iter().enumerate() {
        let read =
            read_fragment(&fragment.bytes, &init.trex).map_err(|e| format!("fragment {n} does not read: {e}"))?;
        if read.sequence != fragment.sequence || fragment.sequence as usize != n + 1 {
            return Err(format!("fragment {n} numbered {} / {}", fragment.sequence, read.sequence));
        }
        let count: usize = read.runs.iter().map(|r| r.samples.len()).sum();
        if count != fragment.samples || count == 0 {
            return Err(format!("fragment {n}: {count} samples read, {} said", fragment.samples));
        }
        let mut independent = true;
        for (index, track) in tracks.iter().enumerate() {
            let run = read.runs.iter().find(|r| r.track_id as usize == index + 1);
            let first = run.and_then(|r| r.samples.first());
            if track.kind == MediaKind::Video && first.is_none_or(|s| s.flags != crate::fragment::SYNC) {
                independent = false;
            }
            if index == fragment.span.track {
                let total: u64 = run.map_or(0, |r| r.samples.iter().map(|s| u64::from(s.duration)).sum());
                if total != fragment.span.duration || first.map(|s| s.decode_time) != Some(fragment.span.start) {
                    return Err(format!("fragment {n}: its span is not its first track's"));
                }
            }
            for sample in run.map(|r| r.samples.as_slice()).unwrap_or_default() {
                if ends.get(index).copied().flatten().is_some_and(|end| end != sample.decode_time) {
                    return Err(format!(
                        "track {index}: a sample at {} after one ending at {:?}",
                        sample.decode_time,
                        ends.get(index)
                    ));
                }
                if sample.duration == 0 {
                    return Err(format!("track {index}: a sample of no duration"));
                }
                let slot = ends.get_mut(index).ok_or("end")?;
                *slot = Some(sample.decode_time + u64::from(sample.duration));
                let at = next.get_mut(index).ok_or("next")?;
                let want =
                    of_track(index).nth(*at).ok_or_else(|| format!("track {index}: more samples than frames"))?;
                *at += 1;
                if want.offset != sample.composition_offset {
                    return Err(format!(
                        "track {index}: offset {} read, {} given",
                        sample.composition_offset, want.offset
                    ));
                }
                let flac = matches!(track.codec, Codec::Pcma | Codec::Pcmu | Codec::L16);
                if flac {
                    let (body, crc) =
                        sample.data.split_at_checked(sample.data.len().saturating_sub(2)).ok_or("short")?;
                    if !body.starts_with(&[0xff, 0xf9]) || crc != crate::flac::crc16(body).to_be_bytes() {
                        return Err(format!("track {index}: a FLAC frame without its sync or CRC"));
                    }
                } else if sample.data != want.data {
                    return Err(format!("track {index}: a sample's bytes differ from its frame's"));
                }
            }
        }
        if independent != fragment.independent {
            return Err(format!(
                "fragment {n} said independent {}, its samples say {independent}",
                fragment.independent
            ));
        }
    }
    for index in 0..tracks.len() {
        if next.get(index).copied() != Some(of_track(index).count()) {
            return Err(format!(
                "track {index}: {:?} samples read of {} frames",
                next.get(index),
                of_track(index).count()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
