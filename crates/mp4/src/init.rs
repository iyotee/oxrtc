//! The init segment: `ftyp`, then `moov` with an empty movie (every sample
//! is in the fragments) - `mvhd`, one `trak` per track, and `mvex` with one
//! `trex` per track (ISO/IEC 14496-12, 8.2 to 8.8).

use oxrtc_core::MediaKind;

use crate::Mp4Error;
use crate::boxes::BoxWriter;
use crate::track::Track;

/// `ftyp`: ISO base media, version 5 (`iso5`: the movie fragments' base data
/// offset is the `moof`), and the brands browsers and players look for.
fn ftyp(w: &mut BoxWriter) -> Result<(), Mp4Error> {
    let open = w.start(b"ftyp");
    w.bytes(b"iso5");
    w.u32(512);
    for brand in [b"iso5", b"iso6", b"mp41"] {
        w.bytes(brand);
    }
    w.end(open)
}

fn mvhd(w: &mut BoxWriter, next_track: u32) -> Result<(), Mp4Error> {
    let open = w.start_full(b"mvhd", 0, 0);
    w.u32(0); // creation_time
    w.u32(0); // modification_time
    w.u32(1000); // timescale
    w.u32(0); // duration: fragmented
    w.u32(0x0001_0000); // rate 1.0
    w.u16(0x0100); // volume 1.0
    w.zeros(10);
    w.matrix();
    w.zeros(24); // pre_defined
    w.u32(next_track);
    w.end(open)
}

fn trak(w: &mut BoxWriter, id: u32, track: &Track) -> Result<(), Mp4Error> {
    let video = track.kind == MediaKind::Video;
    let trak = w.start(b"trak");
    let tkhd = w.start_full(b"tkhd", 0, 0x3); // enabled, in movie
    w.u32(0);
    w.u32(0);
    w.u32(id);
    w.u32(0); // reserved
    w.u32(0); // duration
    w.zeros(8);
    w.u16(0); // layer
    w.u16(0); // alternate_group
    w.u16(if video { 0 } else { 0x0100 });
    w.u16(0);
    w.matrix();
    w.u32(u32::from(track.width) << 16);
    w.u32(u32::from(track.height) << 16);
    w.end(tkhd)?;

    let mdia = w.start(b"mdia");
    let mdhd = w.start_full(b"mdhd", 0, 0);
    w.u32(0);
    w.u32(0);
    w.u32(track.timescale);
    w.u32(0);
    w.u16(0x55c4); // 'und'
    w.u16(0);
    w.end(mdhd)?;
    let hdlr = w.start_full(b"hdlr", 0, 0);
    w.u32(0);
    w.bytes(if video { b"vide" } else { b"soun" });
    w.zeros(12);
    w.bytes(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
    w.end(hdlr)?;

    let minf = w.start(b"minf");
    if video {
        let vmhd = w.start_full(b"vmhd", 0, 1);
        w.zeros(8);
        w.end(vmhd)?;
    } else {
        let smhd = w.start_full(b"smhd", 0, 0);
        w.zeros(4);
        w.end(smhd)?;
    }
    let dinf = w.start(b"dinf");
    let dref = w.start_full(b"dref", 0, 0);
    w.u32(1);
    let url = w.start_full(b"url ", 0, 1); // the data is in this file
    w.end(url)?;
    w.end(dref)?;
    w.end(dinf)?;

    let stbl = w.start(b"stbl");
    let stsd = w.start_full(b"stsd", 0, 0);
    w.u32(1);
    w.bytes(&track.entry);
    w.end(stsd)?;
    for kind in [b"stts", b"stsc", b"stco"] {
        let empty = w.start_full(kind, 0, 0);
        w.u32(0);
        w.end(empty)?;
    }
    let stsz = w.start_full(b"stsz", 0, 0);
    w.u32(0);
    w.u32(0);
    w.end(stsz)?;
    w.end(stbl)?;
    w.end(minf)?;
    w.end(mdia)?;
    w.end(trak)
}

/// The init segment of `tracks`, numbered from 1 in their order.
pub(crate) fn segment(tracks: &[Track]) -> Result<Vec<u8>, Mp4Error> {
    let size: usize = tracks.iter().map(|t| t.entry.len() + 512).sum();
    let mut w = BoxWriter::with_capacity(size + 256);
    ftyp(&mut w)?;
    let moov = w.start(b"moov");
    let count = u32::try_from(tracks.len()).map_err(|_| Mp4Error::TrackCount { count: tracks.len() })?;
    mvhd(&mut w, count + 1)?;
    for (id, track) in (1..).zip(tracks) {
        trak(&mut w, id, track)?;
    }
    let mvex = w.start(b"mvex");
    for id in 1..=count {
        let trex = w.start_full(b"trex", 0, 0);
        w.u32(id);
        w.u32(1); // default_sample_description_index
        w.u32(0);
        w.u32(0);
        w.u32(0);
        w.end(trex)?;
    }
    w.end(mvex)?;
    w.end(moov)?;
    Ok(w.into_inner())
}
