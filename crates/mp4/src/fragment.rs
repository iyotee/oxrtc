//! A media fragment: `moof` (`mfhd`, then one `traf` per track with samples:
//! `tfhd`, `tfdt`, `trun`), then one `mdat` holding every sample, track after
//! track (ISO/IEC 14496-12, 8.8.4 to 8.8.12).

use bytes::Bytes;

use crate::Mp4Error;
use crate::boxes::BoxWriter;

/// A sample ready to be written.
#[derive(Debug, Clone)]
pub(crate) struct Sample {
    pub duration: u32,
    pub flags: u32,
    /// Presentation time minus decode time.
    pub composition_offset: i32,
    pub data: Bytes,
}

/// A track's samples in one fragment.
#[derive(Debug, Clone)]
pub(crate) struct Run<'a> {
    pub track_id: u32,
    pub base_decode_time: u64,
    pub samples: &'a [Sample],
}

/// `sample_depends_on` = 2: a sync sample, decodable alone.
pub(crate) const SYNC: u32 = 0x0200_0000;
/// `sample_depends_on` = 1 and `sample_is_non_sync_sample`.
pub(crate) const NON_SYNC: u32 = 0x0101_0000;

/// `tfhd`: the base data offset is the `moof` (default-base-is-moof).
const TFHD_BASE_IS_MOOF: u32 = 0x02_0000;
/// `trun`: data offset, then per sample duration, size, flags.
const TRUN_FLAGS: u32 = 0x0001 | 0x0100 | 0x0200 | 0x0400;
/// `trun`: per sample composition time offset.
const TRUN_COMPOSITION: u32 = 0x0800;

/// `moof` + `mdat` for `runs`, numbered `sequence`.
pub(crate) fn write(sequence: u32, runs: &[Run<'_>]) -> Result<Vec<u8>, Mp4Error> {
    let data_len: usize = runs.iter().flat_map(|r| r.samples).map(|s| s.data.len()).sum();
    let count: usize = runs.iter().map(|r| r.samples.len()).sum();
    let mut w = BoxWriter::with_capacity(64 + runs.len() * 64 + count * 16 + data_len);
    let moof = w.start(b"moof");
    let mfhd = w.start_full(b"mfhd", 0, 0);
    w.u32(sequence);
    w.end(mfhd)?;
    let mut offsets = Vec::with_capacity(runs.len());
    for run in runs {
        let traf = w.start(b"traf");
        let tfhd = w.start_full(b"tfhd", 0, TFHD_BASE_IS_MOOF);
        w.u32(run.track_id);
        w.end(tfhd)?;
        let tfdt = w.start_full(b"tfdt", 1, 0);
        w.u64(run.base_decode_time);
        w.end(tfdt)?;
        let shifted = run.samples.iter().any(|s| s.composition_offset != 0);
        let flags = if shifted { TRUN_FLAGS | TRUN_COMPOSITION } else { TRUN_FLAGS };
        // Version 1: a signed offset, for a sample shown before it is decoded.
        let trun = w.start_full(b"trun", u8::from(shifted), flags);
        let samples =
            u32::try_from(run.samples.len()).map_err(|_| Mp4Error::invalid("fragment", "too many samples"))?;
        w.u32(samples);
        offsets.push(w.len());
        w.u32(0); // data_offset, written once the moof's size is known
        for sample in run.samples {
            let size = u32::try_from(sample.data.len()).map_err(|_| Mp4Error::TooLarge(sample.data.len()))?;
            w.u32(sample.duration);
            w.u32(size);
            w.u32(sample.flags);
            if shifted {
                w.u32(sample.composition_offset as u32);
            }
        }
        w.end(trun)?;
        w.end(traf)?;
    }
    w.end(moof)?;
    // Each run's data offset: from the moof's first byte to its first sample.
    let mut at = w.len() + 8;
    for (run, field) in runs.iter().zip(offsets) {
        let offset = u32::try_from(at).map_err(|_| Mp4Error::invalid("fragment", "over 4 GiB"))?;
        w.patch_u32(field, offset)?;
        at += run.samples.iter().map(|s| s.data.len()).sum::<usize>();
    }
    let mdat = w.start(b"mdat");
    for sample in runs.iter().flat_map(|r| r.samples) {
        w.bytes(&sample.data);
    }
    w.end(mdat)?;
    Ok(w.into_inner())
}
