//! The JPEG harness: any bytes through the marker reader, the fragments and
//! the packetizer, and what is sent read back by retina's RTP/JPEG
//! depacketizer.

use std::num::NonZeroU32;

use bytes::{Bytes, BytesMut};
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};

use crate::depacketize::{DepacketizerSpec, TrackDepacketizer};
use crate::jpeg::parse;
use crate::packetize::{MAX_PAYLOAD, Packetizer};

/// A real file (FFmpeg's encoder, 48x32, 4:2:0, standard tables): the valid
/// headers a mutated scan is put behind.
pub(crate) const SEED: &[u8] = include_bytes!("../tests/data/yuvj420p.jpg");

/// The seed up to the first byte of its scan.
fn seed_headers() -> &'static [u8] {
    let scan = parse(SEED).map_or(0, |jpeg| jpeg.scan.len());
    SEED.get(..SEED.len().saturating_sub(scan + 2)).unwrap_or_default()
}

/// The first byte picks what the rest is: `0`, a whole file, which the reader
/// accepts or refuses; anything else, a scan behind valid headers, which it
/// accepts. Either way: a file the reader takes is cut in fragments that
/// hold its scan once, in order, within the budget, and what the packetizer
/// writes for it reads back as the same scan; one it refuses writes nothing.
pub fn jpeg_frame(data: &[u8]) -> Result<(), String> {
    let Some((&mode, rest)) = data.split_first() else { return Ok(()) };
    let mut file = Vec::new();
    if mode == 0 {
        file.extend_from_slice(rest);
    } else {
        file.extend_from_slice(seed_headers());
        file.extend_from_slice(rest);
        file.extend_from_slice(&[0xff, 0xd9]);
    }
    let accepted = parse(&file);
    let track = TrackInfo::video(Codec::Jpeg);
    let mut packetizer = Packetizer::new(&track, 26, 0);
    let mut wire = BytesMut::new();
    let frame = Frame::new(0, 0, true, Bytes::from(file.clone()));
    let written = packetizer.packetize(&frame, &CodecParams::None, &mut wire);
    let Ok(jpeg) = accepted else {
        return if written == 0 && wire.is_empty() {
            Ok(())
        } else {
            Err(format!("{written} packets for a refused file"))
        };
    };
    let mut scan = Vec::new();
    let mut count = 0;
    for fragment in jpeg.fragments(MAX_PAYLOAD) {
        if fragment.header().len() + fragment.data.len() > MAX_PAYLOAD {
            return Err("a fragment over the budget".into());
        }
        let [_, high, mid, low] = fragment.header().first_chunk::<4>().map_or([0; 4], |h| *h);
        let offset = usize::from(high) << 16 | usize::from(mid) << 8 | usize::from(low);
        if offset != scan.len() {
            return Err(format!("a fragment at offset {offset} after {} bytes", scan.len()));
        }
        scan.extend_from_slice(fragment.data);
        count += 1;
        if fragment.last != (scan.len() == jpeg.scan.len()) {
            return Err("the last flag is not on the last fragment".into());
        }
    }
    if scan != jpeg.scan {
        return Err(format!("{} bytes of {} sent", scan.len(), jpeg.scan.len()));
    }
    if written != count {
        return Err(format!("{written} packets written, {count} fragments"));
    }
    // Back through the depacketizer the publication side uses.
    let spec = DepacketizerSpec {
        media: "video".into(),
        encoding: "jpeg".into(),
        clock_rate: NonZeroU32::new(90_000).ok_or("clock")?,
        channels: None,
        fmtp: None,
    };
    let mut depacketizer = TrackDepacketizer::new(spec, Codec::Jpeg, 1 << 22).map_err(|e| format!("spec: {e}"))?;
    let mut out = Vec::new();
    let mut wire = wire.as_ref();
    while let [b'$', channel, high, low, rest @ ..] = wire {
        let (packet, after) = rest.split_at_checked(usize::from(u16::from_be_bytes([*high, *low]))).ok_or("cut")?;
        wire = after;
        if *channel == 0 {
            depacketizer.push(packet, &mut out).map_err(|e| format!("our own packet refused: {e}"))?;
        }
    }
    let [frame] = out.as_slice() else { return Err(format!("{} frames out of one", out.len())) };
    match parse(&frame.data) {
        Ok(back) if back.scan == jpeg.scan && back.kind == jpeg.kind && back.tables == jpeg.tables => Ok(()),
        Ok(_) => Err("the picture read back differs".into()),
        Err(error) => Err(format!("the picture read back is refused: {error}")),
    }
}
