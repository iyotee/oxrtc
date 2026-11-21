//! A small reader of fragmented MP4, written from ISO/IEC 14496-12 apart from
//! the writer, for the tests and the fuzz targets: the box tree, the sample
//! entries, and each fragment's samples with their decode times, durations,
//! flags, composition offsets and bytes. It reads what any writer may write
//! (64-bit sizes, defaults in `tfhd` and `trex`, every base data offset
//! rule), refuses what does not add up, and never panics.

/// Most samples one `trun` may declare before the reader refuses it.
pub const MAX_RUN: u32 = 1 << 16;

/// One box: its type, its body, and where it starts in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Atom<'a> {
    /// The four-character type.
    pub kind: [u8; 4],
    /// What follows the header.
    pub body: &'a [u8],
    /// The whole box, header included.
    pub whole: &'a [u8],
    /// Offset of its first byte from the start of the input.
    pub offset: usize,
}

/// A track as the init segment describes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitTrack {
    /// `tkhd`'s track ID.
    pub id: u32,
    /// `hdlr`'s handler type: `vide` or `soun`.
    pub handler: [u8; 4],
    /// `mdhd`'s timescale.
    pub timescale: u32,
    /// The sample entry's type (`avc1`, `fLaC`...).
    pub entry: [u8; 4],
    /// The configuration box inside it (`avcC`, `dfLa`...), whole.
    pub config: Vec<u8>,
    /// A visual entry's size, an audio entry's channels and rate.
    pub width: u16,
    /// Picture height.
    pub height: u16,
    /// Audio channels.
    pub channels: u16,
    /// Audio rate (16.16 integer part).
    pub rate: u16,
}

/// The init segment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Init {
    /// `ftyp`'s major brand.
    pub major: [u8; 4],
    /// The tracks, in `moov` order.
    pub tracks: Vec<InitTrack>,
    /// The track IDs `mvex` has a `trex` for, with its default duration,
    /// size and flags.
    pub trex: Vec<(u32, u32, u32, u32)>,
}

/// One sample of a fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSample {
    /// Its decode time, from `tfdt` and the durations before it.
    pub decode_time: u64,
    /// Its duration.
    pub duration: u32,
    /// Its flags.
    pub flags: u32,
    /// Presentation time minus decode time.
    pub composition_offset: i64,
    /// Its bytes, from `mdat`.
    pub data: Vec<u8>,
}

/// One `traf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRun {
    /// `tfhd`'s track ID.
    pub track_id: u32,
    /// Its samples.
    pub samples: Vec<ReadSample>,
}

/// One `moof` and its `mdat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFragment {
    /// `mfhd`'s sequence number.
    pub sequence: u32,
    /// Its runs, in order.
    pub runs: Vec<ReadRun>,
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("overflow")?;
        let out = self.data.get(self.at..end).ok_or_else(|| format!("{n} bytes past the end at {}", self.at))?;
        self.at = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b.first().copied().unwrap_or(0), b.get(1).copied().unwrap_or(0)]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| "short")?;
        Ok(u32::from_be_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| "short")?;
        Ok(u64::from_be_bytes(b))
    }

    fn rest(&self) -> usize {
        self.data.len().saturating_sub(self.at)
    }
}

/// The boxes of one level of `data`; `base` is `data`'s own offset.
pub fn atoms(data: &[u8], base: usize) -> Result<Vec<Atom<'_>>, String> {
    let mut out = Vec::new();
    let mut c = Cursor::new(data);
    while c.rest() > 0 {
        let start = c.at;
        let size = u64::from(c.u32()?);
        let kind: [u8; 4] = c.take(4)?.try_into().map_err(|_| "short type")?;
        let size = match size {
            0 => u64::try_from(data.len().saturating_sub(start)).map_err(|_| "size")?,
            1 => c.u64()?,
            n => n,
        };
        let header = c.at.saturating_sub(start);
        let size = usize::try_from(size).map_err(|_| "a size over the address space")?;
        if size < header {
            return Err(format!("{} box of {size} bytes, shorter than its header", String::from_utf8_lossy(&kind)));
        }
        let body = c.take(size.saturating_sub(header))?;
        let whole = data.get(start..c.at).ok_or("range")?;
        out.push(Atom { kind, body, whole, offset: base.saturating_add(start) });
    }
    Ok(out)
}

fn child<'a>(atom: &Atom<'a>, kind: &[u8; 4]) -> Result<Atom<'a>, String> {
    atoms(atom.body, atom.offset.saturating_add(8))?
        .into_iter()
        .find(|a| &a.kind == kind)
        .ok_or_else(|| format!("no {} in {}", String::from_utf8_lossy(kind), String::from_utf8_lossy(&atom.kind)))
}

/// A full box's version and flags, and the cursor after them.
fn full<'a>(atom: &Atom<'a>) -> Result<(u8, u32, Cursor<'a>), String> {
    let mut c = Cursor::new(atom.body);
    let word = c.u32()?;
    Ok(((word >> 24) as u8, word & 0x00ff_ffff, c))
}

fn track(trak: &Atom<'_>) -> Result<InitTrack, String> {
    let mut out = InitTrack::default();
    let (version, _, mut c) = full(&child(trak, b"tkhd")?)?;
    c.take(if version == 1 { 16 } else { 8 })?;
    out.id = c.u32()?;
    let mdia = child(trak, b"mdia")?;
    let (version, _, mut c) = full(&child(&mdia, b"mdhd")?)?;
    c.take(if version == 1 { 16 } else { 8 })?;
    out.timescale = c.u32()?;
    let (_, _, mut c) = full(&child(&mdia, b"hdlr")?)?;
    c.take(4)?;
    out.handler = c.take(4)?.try_into().map_err(|_| "short")?;
    let stbl = child(&child(&mdia, b"minf")?, b"stbl")?;
    let (_, _, mut c) = full(&child(&stbl, b"stsd")?)?;
    if c.u32()? == 0 {
        return Err("an stsd with no entry".into());
    }
    let entry = *atoms(c.take(c.rest())?, 0)?.first().ok_or("no sample entry")?;
    out.entry = entry.kind;
    let mut e = Cursor::new(entry.body);
    e.take(8)?; // reserved, data_reference_index
    if &out.handler == b"vide" {
        e.take(16)?;
        out.width = e.u16()?;
        out.height = e.u16()?;
        e.take(50)?;
    } else {
        e.take(8)?;
        out.channels = e.u16()?;
        e.take(6)?;
        out.rate = e.u16()?;
        e.take(2)?;
    }
    let config = *atoms(e.take(e.rest())?, 0)?.first().ok_or("no configuration box")?;
    out.config = config.whole.to_vec();
    Ok(out)
}

/// Reads an init segment: `ftyp`, `moov` with its tracks and `mvex`.
pub fn read_init(data: &[u8]) -> Result<Init, String> {
    let top = atoms(data, 0)?;
    let ftyp = top.iter().find(|a| &a.kind == b"ftyp").ok_or("no ftyp")?;
    let major = ftyp.body.get(..4).ok_or("short ftyp")?.try_into().map_err(|_| "short")?;
    let moov = top.iter().find(|a| &a.kind == b"moov").ok_or("no moov")?;
    let mut init = Init { major, ..Init::default() };
    for atom in atoms(moov.body, 0)? {
        if &atom.kind == b"trak" {
            init.tracks.push(track(&atom)?);
        }
    }
    let mvex = child(moov, b"mvex")?;
    for trex in atoms(mvex.body, 0)?.iter().filter(|a| &a.kind == b"trex") {
        let (_, _, mut c) = full(trex)?;
        let id = c.u32()?;
        c.u32()?;
        init.trex.push((id, c.u32()?, c.u32()?, c.u32()?));
    }
    Ok(init)
}

#[path = "reader_fragment.rs"]
mod fragment;
pub use fragment::read_fragment;
