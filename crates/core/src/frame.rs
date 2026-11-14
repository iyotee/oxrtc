//! Frames, and the only parsing the core does: NAL unit boundaries.
//!
//! H.264 and H.265 frames are carried as a sequence of NAL units, each preceded
//! by its length on four big-endian bytes (the "AVCC" layout that RTP
//! depacketizers and MP4 both produce). [`validate_avcc`] is run on every such
//! frame a driver publishes, so that a consumer can walk the units without
//! re-checking bounds.

use bytes::{BufMut, Bytes, BytesMut};
use tokio::time::Instant;

use crate::error::FrameError;

/// One access unit of one track.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index of the track in the source's (or subscription's) track list.
    pub track: u8,
    /// Presentation timestamp in units of the track's clock rate. Monotonic
    /// within one connection of a source; it restarts when the source does.
    pub pts: i64,
    /// Whether a decoder can start at this frame.
    pub keyframe: bool,
    /// When the frame entered rust2rtc. Kept through relays, for latency.
    pub received: Instant,
    /// The payload. Length-prefixed NAL units for H.264/H.265.
    pub data: Bytes,
}

impl Frame {
    /// A frame received now.
    pub fn new(track: u8, pts: i64, keyframe: bool, data: Bytes) -> Self {
        Self { track, pts, keyframe, received: Instant::now(), data }
    }

    /// The NAL units of an H.264/H.265 frame. Stops at the first malformed
    /// prefix, which a published frame cannot have.
    pub fn nal_units(&self) -> NalUnits<'_> {
        NalUnits { rest: &self.data }
    }

    /// The frame with start codes in place of length prefixes.
    pub fn to_annex_b(&self) -> Bytes {
        avcc_to_annex_b(&self.data)
    }
}

/// Checks that `data` is a non-empty sequence of non-empty length-prefixed NAL
/// units that exactly fills it. Returns the number of units.
pub fn validate_avcc(data: &[u8]) -> Result<usize, FrameError> {
    if data.is_empty() {
        return Err(FrameError::Empty);
    }
    let mut rest = data;
    let mut count = 0;
    while !rest.is_empty() {
        let (prefix, tail) = rest.split_first_chunk::<4>().ok_or(FrameError::TruncatedLength)?;
        let len = u32::from_be_bytes(*prefix) as usize;
        if len == 0 {
            return Err(FrameError::EmptyNal);
        }
        let (_, after) =
            tail.split_at_checked(len).ok_or(FrameError::TruncatedNal { declared: len, available: tail.len() })?;
        rest = after;
        count += 1;
    }
    Ok(count)
}

/// Iterator over the NAL units of a length-prefixed buffer.
#[derive(Debug, Clone)]
pub struct NalUnits<'a> {
    rest: &'a [u8],
}

impl<'a> NalUnits<'a> {
    /// Walks `data`, which should have passed [`validate_avcc`].
    pub fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (prefix, tail) = self.rest.split_first_chunk::<4>()?;
        let len = u32::from_be_bytes(*prefix) as usize;
        match tail.split_at_checked(len) {
            Some((nal, after)) => {
                self.rest = after;
                Some(nal)
            }
            None => {
                self.rest = &[];
                None
            }
        }
    }
}

/// Length prefixes to start codes.
pub fn avcc_to_annex_b(data: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(data.len());
    for nal in NalUnits::new(data) {
        out.put_slice(&[0, 0, 0, 1]);
        out.put_slice(nal);
    }
    out.freeze()
}

/// Start codes to length prefixes. Bytes before the first start code are
/// dropped (a stream without any start code is taken as one unit), as are
/// empty units and the trailing zero bytes of each unit.
pub fn annex_b_to_avcc(data: &[u8]) -> Result<Bytes, FrameError> {
    let mut out = BytesMut::with_capacity(data.len() + 16);
    for nal in AnnexBUnits::new(data) {
        let len = u32::try_from(nal.len()).map_err(|_| FrameError::TruncatedLength)?;
        out.put_u32(len);
        out.put_slice(nal);
    }
    if out.is_empty() {
        return Err(FrameError::Empty);
    }
    Ok(out.freeze())
}

/// Iterator over the NAL units of an Annex B buffer.
#[derive(Debug, Clone)]
pub struct AnnexBUnits<'a> {
    data: &'a [u8],
    next: Option<usize>,
}

impl<'a> AnnexBUnits<'a> {
    /// Walks `data`.
    pub fn new(data: &'a [u8]) -> Self {
        let next = match find_start_code(data, 0) {
            Some(at) => Some(at + 3),
            None if data.is_empty() => None,
            None => Some(0),
        };
        Self { data, next }
    }
}

impl<'a> Iterator for AnnexBUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = self.next?;
            let end = match find_start_code(self.data, start) {
                Some(code) => {
                    self.next = Some(code + 3);
                    code
                }
                None => {
                    self.next = None;
                    self.data.len()
                }
            };
            let unit = self.data.get(start..end).unwrap_or_default();
            let trimmed = trim_trailing_zeros(unit);
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
}

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let hay = data.get(from..)?;
    hay.windows(3).position(|w| w == [0, 0, 1]).map(|at| at + from)
}

fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit.iter().rposition(|&b| b != 0).map_or(0, |at| at + 1);
    unit.get(..end).unwrap_or_default()
}

/// H.264 NAL unit type (`nal_unit_type`, 5 bits).
pub fn h264_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// H.265 NAL unit type (6 bits after the forbidden bit).
pub fn h265_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// Whether an H.264 frame holds an IDR picture.
pub fn h264_is_keyframe(avcc: &[u8]) -> bool {
    NalUnits::new(avcc).any(|nal| h264_nal_type(nal) == Some(5))
}

/// Whether an H.265 frame holds an intra random access point picture (types 16 to 21).
pub fn h265_is_keyframe(avcc: &[u8]) -> bool {
    NalUnits::new(avcc).any(|nal| matches!(h265_nal_type(nal), Some(16..=21)))
}
