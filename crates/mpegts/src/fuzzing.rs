//! The harnesses of the two fuzz targets (`fuzz/`) and of their property
//! tests: any bytes into the reader (`mpegts_reader`), and tracks and frames
//! made from any bytes through the writer and read back (`mpegts_writer`).
//! Each returns an error naming the invariant that broke; a panic is a
//! defect by itself.

use bytes::Bytes;
use oxrtc_core::frame::NalUnits;
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};

use crate::pes::TIME_MASK;
use crate::reader::read;
use crate::{CLOCK, PACKET_SIZE, TIME_OFFSET, Writer};

/// Any bytes into the reader, as they are and with every packet's first
/// byte made a sync byte: a value or an error, and no more payload than input.
pub fn reader_bytes(data: &[u8]) -> Result<(), String> {
    let whole = data.len() - data.len() % PACKET_SIZE;
    let mut synced = data.get(..whole).unwrap_or_default().to_vec();
    for packet in synced.chunks_mut(PACKET_SIZE) {
        if let Some(first) = packet.first_mut() {
            *first = 0x47;
        }
    }
    for input in [data, &synced[..]] {
        if let Ok(stream) = read(input) {
            let total: usize = stream.pes.iter().map(|p| p.data.len()).sum();
            if total > input.len() {
                return Err(format!("{total} bytes of PES read from {} bytes", input.len()));
            }
        }
    }
    Ok(())
}

fn tracks(choice: u8, extra: &[u8]) -> Vec<TrackInfo> {
    let mut tracks = Vec::new();
    let sets = |a: &[u8], b: &[u8]| (Bytes::copy_from_slice(a), Bytes::copy_from_slice(b));
    match choice % 4 {
        1 => {
            let (sps, pps) = sets(&[0x67, 0x42], &[0x68, 0xCE]);
            tracks.push(TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps, pps }));
        }
        2 => {
            let (vps, sps) = sets(&[0x40, 0x01], &[0x42, 0x01]);
            let pps = Bytes::from_static(&[0x44, 0x01]);
            tracks.push(TrackInfo::video(Codec::H265).with_params(CodecParams::H265 { vps, sps, pps }));
        }
        3 => tracks.push(TrackInfo::video(Codec::H264)),
        _ => {}
    }
    match (choice / 4) % 4 {
        1 => tracks.push(
            TrackInfo::audio(Codec::Aac, 48_000, 2)
                .with_params(CodecParams::Aac { config: Bytes::from_static(&[0x11, 0x90]) }),
        ),
        2 => tracks.push(
            TrackInfo::audio(Codec::Aac, 44_100, 1)
                .with_params(CodecParams::Aac { config: Bytes::copy_from_slice(extra) }),
        ),
        3 => tracks.push(TrackInfo::audio(Codec::Opus, 48_000, 1 + u16::from(choice >> 7))),
        _ => {}
    }
    tracks
}

/// The bytes a frame's payload must end with, once read back.
fn carried(info: &TrackInfo, data: &[u8]) -> Vec<u8> {
    match info.codec {
        Codec::H264 | Codec::H265 => NalUnits::new(data).flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat()).collect(),
        _ => data.to_vec(),
    }
}

struct Expected {
    track: usize,
    pts: u64,
    dts: u64,
    tail: Vec<u8>,
}

/// Tracks and frames from any bytes, through the writer, read back: whole
/// packets, counters unbroken, the tables wherever they were asked, each
/// accepted frame one PES on its track's PID, in order, at its time, with its
/// bytes at the end of the payload, a refused frame writing nothing.
pub fn writer_bytes(data: &[u8]) -> Result<(), String> {
    let (&choice, rest) = data.split_first().ok_or("")?;
    let (&extra_len, rest) = rest.split_first().unwrap_or((&0, &[]));
    let (extra, mut rest) = rest.split_at(usize::from(extra_len % 16).min(rest.len()));
    let infos = tracks(choice, extra);
    let Ok(mut writer) = Writer::new(&infos) else { return Ok(()) };
    let mut out = writer.tables().to_vec();
    let mut times = vec![0i64; infos.len()];
    let mut expected = Vec::new();
    while let [op, len, step, tail @ ..] = rest {
        let (payload, after) = tail.split_at(usize::from(*len).min(tail.len()));
        rest = after;
        let track = usize::from(*op) % infos.len();
        let info = infos.get(track).ok_or("no track")?;
        if op & 0x40 != 0 {
            out.extend_from_slice(&writer.tables());
        }
        let frame_data = match info.codec {
            Codec::H264 | Codec::H265 if op & 0x20 == 0 => {
                Bytes::from([&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes()[..], payload].concat())
            }
            _ => Bytes::copy_from_slice(payload),
        };
        let at = times.get_mut(track).ok_or("no time")?;
        *at += i64::from(*step) * 10;
        let shift = i64::from(op & 0x10 != 0) * i64::from(*step);
        let frame = Frame::new(track as u8, *at + shift, op & 0x80 != 0, frame_data.clone());
        let before = out.len();
        match writer.push_decoded(track, &frame, *at) {
            Ok(bytes) => {
                out.extend_from_slice(&bytes);
                let to90 = |t: i64| {
                    (u64::try_from(t).unwrap_or(0) * u64::from(CLOCK) / u64::from(info.clock_rate) + TIME_OFFSET)
                        & TIME_MASK
                };
                expected.push(Expected {
                    track,
                    pts: to90(*at + shift),
                    dts: to90(*at),
                    tail: carried(info, &frame_data),
                });
            }
            Err(_) if out.len() == before => {}
            Err(_) => return Err("a refused frame wrote bytes".to_owned()),
        }
    }
    let stream = read(&out).map_err(|e| format!("the writer's stream does not read back: {e}"))?;
    let pids = writer.pids();
    for (track, pid) in pids.iter().enumerate() {
        let got: Vec<_> = stream.pes.iter().filter(|p| p.pid == *pid).collect();
        let want: Vec<_> = expected.iter().filter(|e| e.track == track).collect();
        if got.len() != want.len() {
            return Err(format!("track {track}: {} PES read for {} frames", got.len(), want.len()));
        }
        for (pes, frame) in got.iter().zip(want) {
            if pes.pts != Some(frame.pts) || pes.dts.unwrap_or(frame.pts) != frame.dts {
                return Err(format!(
                    "track {track}: times {:?}/{:?}, written {}/{}",
                    pes.pts, pes.dts, frame.pts, frame.dts
                ));
            }
            if !pes.data.ends_with(&frame.tail) {
                return Err(format!("track {track}: the frame's bytes are not at the end of its PES"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
