//! A part of an HLS segment (Low-Latency HLS's `EXT-X-PART`): the one being
//! written and the complete ones, each frame's bytes kept with whether they
//! are the lead track's, so that DASH reads each track alone.

use std::ops::Range;
use std::time::Duration;

use bytes::{Bytes, BytesMut};

use super::window::Segment;

/// A complete part.
#[derive(Debug, Clone)]
pub(crate) struct Part {
    pub duration: Duration,
    pub independent: bool,
    pub bytes: Bytes,
    /// Where in `bytes` the frames of the tracks other than the lead are
    /// (each frame its own fMP4 fragment): DASH's one adaptation set per track.
    pub other: Vec<Range<usize>>,
}

impl Part {
    /// The bytes of the lead track's frames alone, or of the others'.
    pub(crate) fn of(&self, lead: bool) -> Vec<Bytes> {
        let mut out = Vec::with_capacity(self.other.len() + 1);
        let mut at = 0;
        for range in &self.other {
            if lead && range.start > at {
                out.push(self.bytes.slice(at..range.start));
            }
            if !lead {
                out.push(self.bytes.slice(range.clone()));
            }
            at = range.end;
        }
        if lead && at < self.bytes.len() {
            out.push(self.bytes.slice(at..));
        }
        out
    }
}

/// The part being written.
#[derive(Debug)]
pub(crate) struct OpenPart {
    pub start: Duration,
    pub independent: bool,
    /// Each chunk, and whether it is the lead track's.
    pub chunks: Vec<(bool, Bytes)>,
    pub size: usize,
}

/// Closes `part` at `end` into segment `open`.
pub(crate) fn segment_of(open: &mut Segment, part: OpenPart, end: Duration) {
    let mut bytes = BytesMut::with_capacity(part.size);
    let mut other: Vec<Range<usize>> = Vec::new();
    for (lead, chunk) in part.chunks {
        let at = bytes.len();
        bytes.extend_from_slice(&chunk);
        if !lead {
            match other.last_mut() {
                Some(last) if last.end == at => last.end = bytes.len(),
                _ => other.push(at..bytes.len()),
            }
        }
    }
    open.parts.push(Part {
        duration: end.saturating_sub(part.start),
        independent: part.independent,
        bytes: bytes.freeze(),
        other,
    });
}

/// Each chunk of a frame, tagged with whether it is the lead track's.
pub(crate) fn tagged(chunks: Vec<Bytes>, lead: bool) -> Vec<(bool, Bytes)> {
    chunks.into_iter().map(|c| (lead, c)).collect()
}
