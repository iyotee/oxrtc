//! Harnesses for the fuzz targets (`fuzz/`, nightly, `cargo fuzz`) and for the
//! property tests that run the same code on stable in `cargo test`
//! ([`fuzzing_tests.rs`](self)).
//!
//! Compiled with the `fuzzing` feature or in tests; never part of a release
//! build. Each harness drives one parser or packetizer of this crate from
//! arbitrary input exactly as the connection does, and returns `Err` naming
//! the invariant that broke. A panic is a defect on its own (`CORE-001`).

use bytes::BytesMut;
use oxrtc_core::{Codec, CodecParams, Frame, MediaKind, TrackInfo, Want};
use rtsp_types::headers::CSEQ;
use rtsp_types::{Message, StatusCode};

use crate::conn::{interleaved, target};
use crate::depacketize::publishable;
use crate::depacketize::{DepacketizerSpec, TrackDepacketizer};
use crate::framing::{Framer, FramingError, Incoming};
use crate::packetize::{MAX_PAYLOAD, Packetizer};
use crate::sdp::{control, describe, parse_announce};
use crate::wire::{encode, reply};

/// Bytes the connection reads from its socket at most at a time.
const READ_CHUNK: usize = 16 * 1024;
/// RTP fixed header.
const RTP_HEADER: usize = 12;
/// Tracks an `ANNOUNCE` may declare in these harnesses.
const MAX_TRACKS: usize = 8;
/// Frame cap handed to depacketizers.
const MAX_FRAME: usize = 1 << 20;
/// Frame cap of the [`depacketize`] harness: small, so that the packets a
/// fuzzer or a property writes do reach it.
const DEPACKETIZE_CAP: usize = 4096;

/// What a parse produced, comparable across two runs: the messages, and
/// whether the connection was closed on a framing error.
type Transcript = (Vec<String>, Option<FramingError>);

fn summary(message: &Incoming) -> String {
    match message {
        Incoming::Request(request) => format!("{request:?}"),
        Incoming::Data { channel, payload } => format!("${channel}:{payload:?}"),
        Incoming::Ignored => "ignored".to_owned(),
    }
}

/// Everything the dispatcher reads off a request, then the response echoing
/// its `CSeq`, which must parse back as one response with the same `CSeq`.
fn request_paths(request: &rtsp_types::Request<Vec<u8>>) -> Result<(), String> {
    let _ = interleaved(request);
    if let Some((_path, query)) = target(request) {
        let _ = Want::from_query(&query);
    }
    let bytes = encode(&reply(request, StatusCode::Ok).build(Vec::new()));
    let (message, consumed) = Message::<Vec<u8>>::parse(bytes.as_ref())
        .map_err(|e| format!("our own response does not parse: {e:?}: {bytes:?}"))?;
    if consumed != bytes.len() {
        return Err(format!("our own response parses short ({consumed} of {}): {bytes:?}", bytes.len()));
    }
    let Message::Response(response) = message else {
        return Err(format!("our own response parses as something else: {bytes:?}"));
    };
    // Surrounding blanks are not part of a header value.
    let sent = request.header(&CSEQ).map(|v| v.as_str().trim().to_owned());
    let echoed = response.header(&CSEQ).map(|v| v.as_str().trim().to_owned());
    if sent != echoed {
        return Err(format!("CSeq {sent:?} echoed as {echoed:?}"));
    }
    Ok(())
}

/// Parses `data` as the connection does: reads of at most `chunk` bytes into
/// the bounded buffer, every complete message taken off after each read.
/// Checks that the buffer never outgrows its bound, that a full buffer always
/// yields a message or an error (never a stall with no room to read), and
/// that every request goes through the dispatcher's header readers.
fn transcript(data: &[u8], max_request: usize, chunk: usize) -> Result<Transcript, String> {
    let mut framer = Framer::new(max_request);
    let limit = framer.limit();
    let mut rest = data;
    let mut seen = Vec::new();
    loop {
        loop {
            match framer.next() {
                Ok(Some(message)) => {
                    if let Incoming::Request(request) = &message {
                        request_paths(request)?;
                    }
                    seen.push(summary(&message));
                }
                Ok(None) => break,
                Err(error) => return Ok((seen, Some(error))),
            }
        }
        let room = framer.room();
        if room > limit {
            return Err(format!("room {room} beyond the bound {limit}"));
        }
        if room == 0 {
            return Err(format!("stall: buffer full at {limit} bytes with no complete message"));
        }
        if rest.is_empty() {
            return Ok((seen, None));
        }
        let take = room.min(READ_CHUNK).min(chunk.max(1)).min(rest.len());
        let (now, later) = rest.split_at_checked(take).unwrap_or((rest, &[]));
        framer.extend(now);
        rest = later;
    }
}

/// The RTSP connection's framing: requests and interleaved frames from
/// `data`, read `chunk` bytes at a time. Beyond the per-run checks of the
/// read loop, the outcome must not depend on how the bytes were split: a
/// request cut across two TCP reads parses as it does in the largest reads
/// the connection makes. (Which error closes a bad connection may differ:
/// a malformed request longer than the limit is "too large" when it arrives
/// slowly and "malformed" when it arrives at once.)
pub fn rtsp_stream(data: &[u8], max_request: usize, chunk: usize) -> Result<(), String> {
    let max_request = max_request.max(1);
    let (split, split_end) = transcript(data, max_request, chunk)?;
    let (whole, whole_end) = transcript(data, max_request, usize::MAX)?;
    if split != whole || split_end.is_some() != whole_end.is_some() {
        return Err(format!(
            "reads of {chunk} bytes gave {split:?} then {split_end:?}, largest reads {whole:?} then {whole_end:?}"
        ));
    }
    Ok(())
}

/// [`rtsp_stream`] with its parameters taken from the first three bytes of
/// `data` (for a fuzzer that hands over bytes only).
pub fn rtsp_stream_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&[size, cut_high, cut_low], rest)) = data.split_first_chunk::<3>() else { return Ok(()) };
    let max_request = 16 + usize::from(size) * 64;
    let chunk = usize::from(u16::from_be_bytes([cut_high, cut_low])).max(1);
    rtsp_stream(rest, max_request, chunk)
}

/// An `ANNOUNCE` body: at most [`MAX_TRACKS`] usable medias, each of a kind
/// matching its codec, each one a codec this server carries, and each one
/// accepted or refused by the depacketizer without a panic.
pub fn sdp_announce(body: &[u8]) -> Result<(), String> {
    let Ok(announced) = parse_announce(body, MAX_TRACKS) else { return Ok(()) };
    if announced.len() > MAX_TRACKS {
        return Err(format!("{} tracks kept, bound {MAX_TRACKS}", announced.len()));
    }
    for track in announced {
        let codec = track.info.codec;
        if codec.kind() != track.info.kind || !publishable(codec) {
            return Err(format!("announced track kept against its kind or support: {:?}", track.info));
        }
        let _ = TrackDepacketizer::new(track.spec, codec, MAX_FRAME);
    }
    Ok(())
}

/// Tracks built from fuzzer bytes, five bytes each: codec, clock rate (three
/// bytes, never zero), channels.
pub fn tracks_from_bytes(data: &[u8]) -> Vec<TrackInfo> {
    data.as_chunks::<5>()
        .0
        .iter()
        .take(16)
        .filter_map(|&[codec, high, mid, low, channels]| {
            let codec = *Codec::ALL.get(usize::from(codec) % Codec::ALL.len())?;
            let clock = u32::from_be_bytes([0, high, mid, low]).max(1);
            Some(match codec.kind() {
                MediaKind::Video => TrackInfo { clock_rate: clock, ..TrackInfo::video(codec) },
                MediaKind::Audio => TrackInfo::audio(codec, clock, u16::from(channels % 9)),
            })
        })
        .collect()
}

/// `DESCRIBE`'s SDP read back by `ANNOUNCE`'s parser: every track this server
/// can packetize comes back, in order, with its codec, kind and control; and
/// its clock rate and channel count unless the encoding fixes them (Opus is
/// always `opus/48000/2`).
pub fn sdp_roundtrip(tracks: &[TrackInfo]) -> Result<(), String> {
    let sdp = describe(tracks);
    let back = parse_announce(&sdp, usize::MAX).map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&sdp)))?;
    let expected: Vec<(usize, &TrackInfo)> = tracks.iter().enumerate().filter(|(_, t)| publishable(t.codec)).collect();
    if back.len() != expected.len() {
        return Err(format!("{} tracks described, {} read back: {}", expected.len(), back.len(), lossy(&sdp)));
    }
    for ((index, sent), got) in expected.into_iter().zip(back) {
        let info = &got.info;
        let same_shape = info.codec == sent.codec && info.kind == sent.kind;
        let fixed = sent.codec == Codec::Opus;
        let same_rate = fixed || info.clock_rate == sent.clock_rate;
        let same_channels = fixed || sent.kind == MediaKind::Video || info.channels == sent.channels.max(1);
        if !(same_shape && same_rate && same_channels) || got.control != Some(control(index)) {
            return Err(format!("track {index} {sent:?} read back as {info:?} ({:?}): {}", got.control, lossy(&sdp)));
        }
    }
    Ok(())
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The depacketizers a publication can use, by index.
pub const DEPACKETIZERS: usize = 6;

fn depacketizer_spec(which: usize) -> (DepacketizerSpec, Codec) {
    let (media, encoding, clock, channels, codec, fmtp) = match which % DEPACKETIZERS {
        0 => ("video", "h264", 90_000, None, Codec::H264, None),
        1 => ("video", "h265", 90_000, None, Codec::H265, None),
        2 => ("audio", "opus", 48_000, std::num::NonZeroU16::new(2), Codec::Opus, None),
        3 => ("audio", "pcmu", 8000, None, Codec::Pcmu, None),
        4 => ("audio", "l16", 44_100, std::num::NonZeroU16::new(2), Codec::L16, None),
        _ => (
            "audio",
            "mpeg4-generic",
            16_000,
            None,
            Codec::Aac,
            Some(
                "streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1408",
            ),
        ),
    };
    let clock_rate = std::num::NonZeroU32::new(clock).unwrap_or(std::num::NonZeroU32::MIN);
    let spec = DepacketizerSpec {
        media: media.to_owned(),
        encoding: encoding.to_owned(),
        clock_rate,
        channels,
        fmtp: fmtp.map(str::to_owned),
    };
    (spec, codec)
}

/// RTP packets from a peer into depacketizer `which` (see [`DEPACKETIZERS`]):
/// errors are answers, panics are defects, and no frame that comes out is
/// over the cap ([`DEPACKETIZE_CAP`], parameter sets put in band included).
pub fn depacketize(which: usize, packets: &[Vec<u8>]) -> Result<(), String> {
    let (spec, codec) = depacketizer_spec(which);
    let mut d = TrackDepacketizer::new(spec, codec, DEPACKETIZE_CAP).map_err(|e| format!("spec {which}: {e}"))?;
    let mut out = Vec::new();
    for packet in packets {
        let _ = d.push(packet, &mut out);
    }
    match out.iter().find(|f| f.data.len() > DEPACKETIZE_CAP) {
        Some(frame) => Err(format!("{} byte frame, cap {DEPACKETIZE_CAP}", frame.data.len())),
        None => Ok(()),
    }
}

/// [`depacketize`] from bytes: the first selects the depacketizer, then each
/// packet is a 16-bit length and that many bytes.
pub fn depacketize_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&which, mut rest)) = data.split_first() else { return Ok(()) };
    let mut packets = Vec::new();
    while let Some((&[high, low], tail)) = rest.split_first_chunk::<2>() {
        let len = usize::from(u16::from_be_bytes([high, low])).min(tail.len());
        let (packet, after) = tail.split_at_checked(len).unwrap_or((tail, &[]));
        packets.push(packet.to_vec());
        rest = after;
    }
    depacketize(usize::from(which), &packets)
}

/// The interleaved packets of `wire`, with their channels, split by the
/// connection's own framing.
fn interleaved_packets(wire: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let mut framer = Framer::new(1);
    framer.extend(wire);
    let mut packets = Vec::new();
    loop {
        match framer.next() {
            Ok(Some(Incoming::Data { channel, payload })) => packets.push((channel, payload.to_vec())),
            Ok(None) if framer.room() == framer.limit() => return Ok(packets),
            other => return Err(format!("packetizer output is not interleaved RTP: {other:?}")),
        }
    }
}

/// `body` with emulation prevention, as an encoder writes it: never two zero
/// bytes followed by a byte up to 3, which would read as a start code.
fn escape_into(body: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &byte in body {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
}

fn avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&u32::try_from(nal.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Frames this server packetizes, depacketized again by the publication side
/// (retina): what went in comes out. Video is a non-keyframe of slice NAL
/// units (`bodies`, each behind a header for `codec`); audio is the bodies
/// concatenated as one frame. Video and G.711/L16 packets also stay within
/// the payload budget.
pub fn packetize_roundtrip(codec: Codec, bodies: &[Vec<u8>]) -> Result<(), String> {
    let (spec, depacketizer_codec) = match codec {
        Codec::H264 => depacketizer_spec(0),
        Codec::H265 => depacketizer_spec(1),
        Codec::Opus => depacketizer_spec(2),
        Codec::Pcmu => depacketizer_spec(3),
        Codec::L16 => depacketizer_spec(4),
        Codec::Aac => depacketizer_spec(5),
        Codec::Pcma => {
            let (spec, _) = depacketizer_spec(3);
            (DepacketizerSpec { encoding: "pcma".to_owned(), ..spec }, Codec::Pcma)
        }
        // Its own harness, `jpeg_frame`: a frame is a whole file there.
        Codec::Jpeg => return Ok(()),
    };
    let track = match codec.kind() {
        MediaKind::Video => TrackInfo::video(codec),
        MediaKind::Audio => {
            TrackInfo::audio(codec, spec.clock_rate.get(), spec.channels.map_or(1, std::num::NonZeroU16::get))
        }
    };
    let (sent, data) = match codec {
        Codec::H264 | Codec::H265 => {
            // Non-IDR slice headers (type 1 for H.264, TRAIL_R for H.265).
            let header: &[u8] = if codec == Codec::H264 { &[0x41] } else { &[0x02, 0x01] };
            let nals: Vec<Vec<u8>> = bodies
                .iter()
                .filter(|b| !b.is_empty())
                .map(|b| {
                    let mut nal = header.to_vec();
                    escape_into(b, &mut nal);
                    // A NAL unit ends on its stop bit: trailing zero bytes are
                    // not part of it, and the depacketizer drops them.
                    if let Some(last) = nal.last_mut()
                        && *last == 0
                    {
                        *last = 0x80;
                    }
                    nal
                })
                .collect();
            if nals.is_empty() {
                return Ok(());
            }
            let data = avcc(&nals);
            (nals, data)
        }
        _ => {
            let mut data = bodies.concat();
            if codec == Codec::L16 {
                data.truncate(data.len() - data.len() % 4);
            }
            if codec == Codec::Opus
                && let Some(toc) = data.first_mut()
            {
                // Frame-count code 0: one frame, which any length is.
                *toc &= 0xfc;
            }
            if data.is_empty() {
                return Ok(());
            }
            (vec![data.clone()], data)
        }
    };
    let frame = Frame::new(0, 0, false, bytes::Bytes::from(data));
    let mut packetizer = Packetizer::new(&track, 96, 0);
    let mut wire = BytesMut::new();
    let written = packetizer.packetize(&frame, &CodecParams::None, &mut wire);
    let on_wire = interleaved_packets(&wire)?;
    // The first packet sent comes after its sender report (RTCP channel 1);
    // nothing sent, nothing on the wire at all.
    let media = match on_wire.split_first() {
        Some(((1, _), media)) if written > 0 => media,
        Some(_) if written > 0 => return Err("the first packet has no sender report before it".into()),
        _ => on_wire.as_slice(),
    };
    if media.iter().any(|(channel, _)| *channel != 0) {
        return Err("RTCP among the RTP packets".into());
    }
    let packets: Vec<Vec<u8>> = media.iter().map(|(_, packet)| packet.clone()).collect();
    if packets.len() != written {
        return Err(format!("{written} packets reported, {} on the wire", packets.len()));
    }
    let budgeted = matches!(codec, Codec::H264 | Codec::H265 | Codec::Pcma | Codec::Pcmu | Codec::L16);
    if budgeted && let Some(big) = packets.iter().find(|p| p.len() > RTP_HEADER + MAX_PAYLOAD) {
        return Err(format!("{} byte packet, budget {}", big.len(), RTP_HEADER + MAX_PAYLOAD));
    }
    if written == 0 {
        // Refused whole (an AAC frame its 13-bit size field cannot describe,
        // an Opus frame over the interleaved length): nothing half-sent.
        return Ok(());
    }
    let mut d = TrackDepacketizer::new(spec, depacketizer_codec, MAX_FRAME).map_err(|e| format!("spec: {e}"))?;
    let mut out = Vec::new();
    for packet in &packets {
        d.push(packet, &mut out).map_err(|e| format!("our own packet refused: {e}"))?;
    }
    let got: Vec<Vec<u8>> = match codec {
        Codec::H264 | Codec::H265 => {
            let frames: Vec<&[u8]> = out.iter().map(|f| f.data.as_ref()).collect();
            let [single] = frames.as_slice() else {
                return Err(format!("{} frames out of one", frames.len()));
            };
            oxrtc_core::frame::NalUnits::new(single).map(<[u8]>::to_vec).collect()
        }
        _ => vec![out.iter().flat_map(|f| f.data.iter().copied()).collect()],
    };
    if got != sent {
        return Err(format!("{codec:?}: sent {sent:?}, got {got:?} back"));
    }
    Ok(())
}

/// [`packetize_roundtrip`] from bytes: the first picks the codec, the next
/// cuts the rest into bodies of a multiple of 64 bytes.
pub fn packetize_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&pick, rest)) = data.split_first() else { return Ok(()) };
    let Some(&codec) = Codec::ALL.get(usize::from(pick) % Codec::ALL.len()) else { return Ok(()) };
    let (cut, body) = rest.split_first().map_or((1, rest), |(&c, b)| (usize::from(c).max(1) * 64, b));
    let bodies: Vec<Vec<u8>> = body.chunks(cut).map(<[u8]>::to_vec).collect();
    packetize_roundtrip(codec, &bodies)
}

#[path = "fuzzing_jpeg.rs"]
mod jpeg_harness;
pub use jpeg_harness::jpeg_frame;

/// The valid file the harness builds on, for the property tests that mutate it.
#[cfg(test)]
pub(crate) fn jpeg_harness_seed() -> &'static [u8] {
    jpeg_harness::SEED
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
