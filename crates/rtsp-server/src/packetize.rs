//! Frames to RTP packets, framed for TCP interleaving, and RTCP sender reports.
//!
//! H.264 (RFC 6184) as single NAL units and FU-A, H.265 (RFC 7798) as single
//! NAL units and FU, AAC as RFC 3640 `AAC-hbr` with one AU header, the other
//! audio codecs one frame per packet (G.711 and L16 split on sample bounds when
//! a frame exceeds the payload size).
//!
//! RTP time is the hub's timeline ([`oxrtc_core::timeline`]) plus a random
//! offset: it never jumps when a source restarts. A track's first packet
//! comes after a sender report that dates it by when its frame was received
//! ([`Frame::received`]: a frame of the cached GOP by when the camera sent
//! it, not by when this reader got it); every later report follows from that
//! one along the timeline. A reader that dates packets by the reports (FFmpeg,
//! as soon as a stream has two tracks) then places picture and sound by the
//! same clock from the first packet on - before, it dated each track from its
//! own first packet until the first periodic report, 5 s later, moved them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use oxrtc_core::frame::{h264_nal_type, h265_nal_type};
use oxrtc_core::timeline::ticks;
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};
use tokio::time::Instant;

use crate::wire::interleaved_header;

/// Largest RTP payload: fits a 1500-byte MTU with room for tunnels.
pub(crate) const MAX_PAYLOAD: usize = 1200;

const RTP_HEADER: usize = 12;
const FU_A: u8 = 28;
const H265_FU: u8 = 49;
const START: u8 = 0x80;
const END: u8 = 0x40;
/// Seconds between 1900 (NTP) and 1970 (Unix).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// The RTP payload type of a track: the static one for 8 kHz mono G.711,
/// a dynamic one otherwise.
pub(crate) fn payload_type(index: usize, track: &TrackInfo) -> u8 {
    match (track.codec, track.clock_rate, track.channels) {
        (Codec::Pcmu, 8000, 1) => 0,
        (Codec::Pcma, 8000, 1) => 8,
        _ => 96u8.saturating_add(u8::try_from(index).unwrap_or(u8::MAX)).min(127),
    }
}

/// Whether this server can packetize the codec.
pub(crate) fn supported(codec: Codec) -> bool {
    !matches!(codec, Codec::Jpeg)
}

/// The RTP state of one track of one reader.
#[derive(Debug)]
pub(crate) struct Packetizer {
    codec: Codec,
    clock_rate: u32,
    sample_bytes: usize,
    payload_type: u8,
    ssrc: u32,
    seq: u16,
    initial_seq: u16,
    ts_offset: u32,
    channel: u8,
    rtcp_channel: u8,
    packets: u32,
    octets: u32,
    /// The timeline time of the last frame sent.
    last_pts: i64,
    /// The first frame sent: its timeline time and the wall-clock time it was
    /// received. Every sender report is derived from it.
    first: Option<(i64, SystemTime)>,
}

impl Packetizer {
    /// A packetizer sending on interleaved `channel` (RTCP on `channel + 1`),
    /// with a random SSRC, sequence start and timestamp offset.
    pub(crate) fn new(track: &TrackInfo, payload_type: u8, channel: u8) -> Self {
        let seq = fastrand::u16(..);
        let sample_bytes = match track.codec {
            Codec::L16 => 2 * usize::from(track.channels.max(1)),
            _ => usize::from(track.channels.max(1)),
        };
        Self {
            codec: track.codec,
            clock_rate: track.clock_rate.max(1),
            sample_bytes,
            payload_type,
            ssrc: fastrand::u32(..),
            seq,
            initial_seq: seq,
            ts_offset: fastrand::u32(..),
            channel,
            rtcp_channel: channel.wrapping_add(1),
            packets: 0,
            octets: 0,
            last_pts: 0,
            first: None,
        }
    }

    /// Sends the sender reports on `channel` instead of the RTP channel + 1
    /// (a client may choose any pair, `interleaved=4-7`).
    pub(crate) fn with_rtcp_channel(mut self, channel: u8) -> Self {
        self.rtcp_channel = channel;
        self
    }

    /// The interleaved RTP channel.
    pub(crate) fn channel(&self) -> u8 {
        self.channel
    }

    /// The interleaved RTCP channel.
    pub(crate) fn rtcp_channel(&self) -> u8 {
        self.rtcp_channel
    }

    /// The sequence number of the first packet, for `RTP-Info`.
    pub(crate) fn initial_seq(&self) -> u16 {
        self.initial_seq
    }

    /// The SSRC, for the `Transport` header.
    pub(crate) fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The RTP timestamp of timeline time `pts`.
    fn rtp_ts(&self, pts: i64) -> u32 {
        // RTP timestamps are the clock modulo 2^32: truncation is the intent.
        self.ts_offset.wrapping_add(pts as u32)
    }

    /// Appends the packets of `frame` to `out`, after a sender report when it
    /// is the track's first. Returns how many RTP packets were written.
    pub(crate) fn packetize(&mut self, frame: &Frame, params: &CodecParams, out: &mut BytesMut) -> usize {
        let ts = self.rtp_ts(frame.pts);
        let start = out.len();
        let first = self.first.is_none().then(|| (frame.pts, wallclock(frame.received)));
        if let Some((_, wall)) = first {
            self.report(wall, ts, out);
        }
        let written = match self.codec {
            Codec::H264 | Codec::H265 => self.video(frame, params, ts, out),
            Codec::Aac => self.aac(&frame.data, ts, out),
            Codec::Pcma | Codec::Pcmu | Codec::L16 => self.samples(&frame.data, ts, out),
            Codec::Opus => usize::from(self.packet(out, ts, true, &[&frame.data])),
            Codec::Jpeg => 0,
        };
        if written == 0 {
            // Nothing sent: no report either, the next frame is the first.
            out.truncate(start);
            return 0;
        }
        if first.is_some() {
            self.first = first;
        }
        self.last_pts = frame.pts;
        written
    }

    fn video(&mut self, frame: &Frame, params: &CodecParams, ts: u32, out: &mut BytesMut) -> usize {
        let mut nals: Vec<&[u8]> = Vec::new();
        if frame.keyframe {
            self.parameter_sets(frame, params, &mut nals);
        }
        nals.extend(frame.nal_units());
        let count = nals.len();
        let mut written = 0;
        for (index, nal) in nals.into_iter().enumerate() {
            written += self.nal(nal, ts, index + 1 == count, out);
        }
        written
    }

    /// The parameter sets `params` holds and the frame does not carry.
    fn parameter_sets<'a>(&self, frame: &Frame, params: &'a CodecParams, nals: &mut Vec<&'a [u8]>) {
        let carried = |is: &dyn Fn(&[u8]) -> bool| frame.nal_units().any(is);
        match (self.codec, params) {
            (Codec::H264, CodecParams::H264 { sps, pps }) => {
                if !carried(&|n| h264_nal_type(n) == Some(7)) {
                    nals.push(sps);
                }
                if !carried(&|n| h264_nal_type(n) == Some(8)) {
                    nals.push(pps);
                }
            }
            (Codec::H265, CodecParams::H265 { vps, sps, pps }) => {
                for (set, kind) in [(vps, 32), (sps, 33), (pps, 34)] {
                    if !carried(&|n| h265_nal_type(n) == Some(kind)) {
                        nals.push(set);
                    }
                }
            }
            _ => {}
        }
        nals.retain(|n| !n.is_empty());
    }

    fn nal(&mut self, nal: &[u8], ts: u32, last: bool, out: &mut BytesMut) -> usize {
        if nal.len() <= MAX_PAYLOAD {
            return usize::from(self.packet(out, ts, last, &[nal]));
        }
        // Fragmentation: the NAL header is replaced by the FU indicator/header.
        let (prefix, body): (Vec<u8>, &[u8]) = match self.codec {
            Codec::H264 => match nal.split_first() {
                Some((&hdr, body)) => (vec![(hdr & 0xe0) | FU_A, hdr & 0x1f], body),
                None => return 0,
            },
            _ => match nal.split_first_chunk::<2>() {
                Some((&[h0, h1], body)) => (vec![(h0 & 0x81) | (H265_FU << 1), h1, (h0 >> 1) & 0x3f], body),
                None => return 0,
            },
        };
        let Some((&fu_type, indicator)) = prefix.split_last() else { return 0 };
        let chunk = MAX_PAYLOAD - prefix.len();
        let pieces = body.len().div_ceil(chunk);
        let mut written = 0;
        for (index, piece) in body.chunks(chunk).enumerate() {
            let mut fu = fu_type;
            if index == 0 {
                fu |= START;
            }
            let end = index + 1 == pieces;
            if end {
                fu |= END;
            }
            written += usize::from(self.packet(out, ts, last && end, &[indicator, &[fu], piece]));
        }
        written
    }

    fn aac(&mut self, data: &[u8], ts: u32, out: &mut BytesMut) -> usize {
        // AU-headers-length (16 bits), then one header: 13-bit size, 3-bit index.
        let Ok(size) = u16::try_from(data.len()) else { return 0 };
        if size >= 1 << 13 {
            return 0;
        }
        let header = [0x00, 0x10, (size >> 5) as u8, ((size & 0x1f) << 3) as u8];
        usize::from(self.packet(out, ts, true, &[&header, data]))
    }

    fn samples(&mut self, data: &[u8], ts: u32, out: &mut BytesMut) -> usize {
        let chunk = (MAX_PAYLOAD / self.sample_bytes).max(1) * self.sample_bytes;
        let mut written = 0;
        let mut offset = 0u32;
        for piece in data.chunks(chunk) {
            written += usize::from(self.packet(out, ts.wrapping_add(offset), false, &[piece]));
            offset = offset.wrapping_add(u32::try_from(piece.len() / self.sample_bytes).unwrap_or(0));
        }
        written
    }

    /// Writes one interleaved RTP packet made of `parts`.
    fn packet(&mut self, out: &mut BytesMut, ts: u32, marker: bool, parts: &[&[u8]]) -> bool {
        let payload: usize = parts.iter().map(|p| p.len()).sum();
        if !interleaved_header(out, self.channel, RTP_HEADER + payload) {
            return false;
        }
        out.extend_from_slice(&[0x80, (u8::from(marker) << 7) | (self.payload_type & 0x7f)]);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        for part in parts {
            out.extend_from_slice(part);
        }
        self.seq = self.seq.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(u32::try_from(payload).unwrap_or(u32::MAX));
        true
    }

    /// Appends an RTCP sender report for `now` (on the RTCP channel). Nothing
    /// before the first packet. Its RTP time follows from the first packet's
    /// report along the timeline, not from when the last packet left: a
    /// camera that delivers in bursts would move it at every report.
    pub(crate) fn sender_report(&self, now: SystemTime, out: &mut BytesMut) {
        let Some((first_pts, first_wall)) = self.first else { return };
        let last_wall = shifted(first_wall, self.last_pts.saturating_sub(first_pts), self.clock_rate);
        let since_last = ticks_between(last_wall, now, self.clock_rate);
        self.report(now, self.rtp_ts(self.last_pts.saturating_add(since_last)), out);
    }

    /// Appends a sender report saying that `wall` is RTP time `rtp`.
    fn report(&self, wall: SystemTime, rtp: u32, out: &mut BytesMut) {
        let unix = wall.duration_since(UNIX_EPOCH).unwrap_or_default();
        // NTP seconds are modulo 2^32 as well.
        let ntp_secs = (unix.as_secs() + NTP_UNIX_OFFSET) as u32;
        let ntp_frac = ((u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000) as u32;
        if !interleaved_header(out, self.rtcp_channel, 28) {
            return;
        }
        out.extend_from_slice(&[0x80, 200, 0, 6]);
        for word in [self.ssrc, ntp_secs, ntp_frac, rtp, self.packets, self.octets] {
            out.extend_from_slice(&word.to_be_bytes());
        }
    }
}

/// The wall-clock time of a monotonic instant.
fn wallclock(at: Instant) -> SystemTime {
    let now = SystemTime::now();
    now.checked_sub(Instant::now().saturating_duration_since(at)).unwrap_or(now)
}

/// `wall` moved by `count` ticks of a `rate` Hz clock (back when negative).
fn shifted(wall: SystemTime, count: i64, rate: u32) -> SystemTime {
    let nanos = u128::from(count.unsigned_abs()) * 1_000_000_000 / u128::from(rate.max(1));
    let span = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
    let moved = if count >= 0 { wall.checked_add(span) } else { wall.checked_sub(span) };
    moved.unwrap_or(wall)
}

/// `later - earlier` in ticks of a `rate` Hz clock, negative when `later` is earlier.
fn ticks_between(earlier: SystemTime, later: SystemTime, rate: u32) -> i64 {
    match later.duration_since(earlier) {
        Ok(ahead) => ticks(ahead, rate),
        Err(behind) => ticks(behind.duration(), rate).saturating_neg(),
    }
}

#[cfg(test)]
#[path = "packetize_tests.rs"]
mod tests;
