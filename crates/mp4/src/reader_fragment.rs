//! The fragment half of the test reader: `moof` and `mdat`.

use super::{Atom, MAX_RUN, ReadFragment, ReadRun, ReadSample, atoms, child, full};

/// `tfhd` with what it says or leaves to `trex`.
struct Header {
    track_id: u32,
    base: Option<u64>,
    base_is_moof: bool,
    duration: u32,
    size: u32,
    flags: u32,
}

fn header(traf: &Atom<'_>, trex: &[(u32, u32, u32, u32)]) -> Result<Header, String> {
    let (_, flags, mut c) = full(&child(traf, b"tfhd")?)?;
    let track_id = c.u32()?;
    let defaults = trex.iter().find(|t| t.0 == track_id).map_or((0, 0, 0), |t| (t.1, t.2, t.3));
    let base = if flags & 0x01 != 0 { Some(c.u64()?) } else { None };
    if flags & 0x02 != 0 {
        c.u32()?; // sample_description_index
    }
    let duration = if flags & 0x08 != 0 { c.u32()? } else { defaults.0 };
    let size = if flags & 0x10 != 0 { c.u32()? } else { defaults.1 };
    let sample_flags = if flags & 0x20 != 0 { c.u32()? } else { defaults.2 };
    Ok(Header { track_id, base, base_is_moof: flags & 0x02_0000 != 0, duration, size, flags: sample_flags })
}

/// Reads one fragment: a `moof` and the `mdat` after it, with the `trex`
/// defaults of the init segment.
pub fn read_fragment(data: &[u8], trex: &[(u32, u32, u32, u32)]) -> Result<ReadFragment, String> {
    let top = atoms(data, 0)?;
    let moof = top.iter().find(|a| &a.kind == b"moof").ok_or("no moof")?;
    let mdat = top.iter().find(|a| &a.kind == b"mdat").ok_or("no mdat")?;
    let mdat_start = mdat.offset + (mdat.whole.len() - mdat.body.len());
    let mdat_end = mdat.offset + mdat.whole.len();
    let (_, _, mut c) = full(&child(moof, b"mfhd")?)?;
    let sequence = c.u32()?;
    let mut runs = Vec::new();
    // Every sample's bytes, to refuse two samples that share some: each byte
    // of an mdat belongs to one sample at most.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // Where data goes when nothing says: the moof, then each run's end.
    let mut implicit = moof.offset as u64;
    for traf in atoms(moof.body, moof.offset + 8)?.iter().filter(|a| &a.kind == b"traf") {
        let h = header(traf, trex)?;
        let base = h.base.unwrap_or(if h.base_is_moof || runs.is_empty() { moof.offset as u64 } else { implicit });
        let mut decode_time = match child(traf, b"tfdt") {
            Ok(tfdt) => {
                let (version, _, mut c) = full(&tfdt)?;
                if version == 1 { c.u64()? } else { u64::from(c.u32()?) }
            }
            Err(_) => 0,
        };
        let mut samples = Vec::new();
        for trun in atoms(traf.body, traf.offset + 8)?.iter().filter(|a| &a.kind == b"trun") {
            let (version, flags, mut c) = full(trun)?;
            let count = c.u32()?;
            if count > MAX_RUN {
                return Err(format!("a trun of {count} samples"));
            }
            let offset = if flags & 0x01 != 0 { i64::from(c.u32()? as i32) } else { 0 };
            let first_flags = if flags & 0x04 != 0 { Some(c.u32()?) } else { None };
            let per_sample = [0x100, 0x200, 0x400, 0x800].iter().filter(|&&f| flags & f != 0).count() * 4;
            if per_sample.saturating_mul(count as usize) > c.rest() {
                return Err("a trun shorter than its samples".into());
            }
            let mut at = i64::try_from(base).map_err(|_| "base")?.checked_add(offset).ok_or("offset")?;
            if flags & 0x01 == 0 && !samples.is_empty() {
                at = i64::try_from(implicit).map_err(|_| "implicit")?;
            }
            for i in 0..count {
                let duration = if flags & 0x100 != 0 { c.u32()? } else { h.duration };
                let size = if flags & 0x200 != 0 { c.u32()? } else { h.size };
                let mut sample_flags = if flags & 0x400 != 0 { c.u32()? } else { h.flags };
                if i == 0
                    && let Some(first) = first_flags
                {
                    sample_flags = first;
                }
                let composition_offset = if flags & 0x800 != 0 {
                    let raw = c.u32()?;
                    if version == 0 { i64::from(raw) } else { i64::from(raw as i32) }
                } else {
                    0
                };
                let start = usize::try_from(at).map_err(|_| "a sample before the input")?;
                let end = start.checked_add(size as usize).ok_or("overflow")?;
                if start < mdat_start || end > mdat_end {
                    return Err(format!("sample bytes {start}..{end} outside the mdat {mdat_start}..{mdat_end}"));
                }
                let bytes = data.get(start..end).ok_or("sample range")?.to_vec();
                ranges.push((start, end));
                samples.push(ReadSample {
                    decode_time,
                    duration,
                    flags: sample_flags,
                    composition_offset,
                    data: bytes,
                });
                decode_time = decode_time.checked_add(u64::from(duration)).ok_or("decode time overflow")?;
                at = i64::try_from(end).map_err(|_| "end")?;
                implicit = end as u64;
            }
        }
        runs.push(ReadRun { track_id: h.track_id, samples });
    }
    ranges.sort_unstable();
    if let Some(pair) = ranges.windows(2).find(|pair| matches!(pair, [a, b] if b.0 < a.1)) {
        return Err(format!("samples sharing bytes: {pair:?}"));
    }
    Ok(ReadFragment { sequence, runs })
}
