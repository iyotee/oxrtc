//! The reading side of a session's [`Window`]: its segments and parts as
//! the playlists, the manifest and the media routes ask for them.

use std::time::Duration;

use bytes::Bytes;

use super::{Segment, Window};

impl Window {
    /// The bytes held, every segment and part.
    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// The complete segments, oldest first.
    pub(crate) fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.done.iter()
    }

    /// The segment being written, with its complete parts.
    pub(crate) fn open(&self) -> Option<&Segment> {
        self.open.as_ref()
    }

    /// The longest segment yet, at least the target.
    pub(crate) fn longest(&self) -> Duration {
        self.longest.max(self.settings.segment)
    }

    /// The bytes of complete segment `msn`, if it is in the window.
    pub(crate) fn segment(&self, msn: u64) -> Option<Vec<Bytes>> {
        let segment = self.done.iter().find(|s| s.msn == msn)?;
        Some(segment.parts.iter().map(|p| p.bytes.clone()).collect())
    }

    /// The bytes of complete segment `msn`'s lead track alone (`lead`), or of
    /// its other tracks: DASH's segments of one adaptation set.
    pub(crate) fn segment_of_track(&self, msn: u64, lead: bool) -> Option<Vec<Bytes>> {
        let segment = self.done.iter().find(|s| s.msn == msn)?;
        Some(segment.parts.iter().flat_map(|p| p.of(lead)).collect())
    }

    /// The bytes of part `index` of segment `msn`, once complete.
    pub(crate) fn part(&self, msn: u64, index: usize) -> Option<Bytes> {
        let segment = self.done.iter().chain(self.open.iter()).find(|s| s.msn == msn)?;
        segment.parts.get(index).map(|p| p.bytes.clone())
    }

    /// The latest complete segment's number.
    pub(crate) fn last_segment(&self) -> Option<u64> {
        self.done.back().map(|s| s.msn)
    }

    /// The latest complete part: its segment's number and its index.
    pub(crate) fn last_part(&self) -> Option<(u64, usize)> {
        if let Some(open) = &self.open
            && let Some(last) = open.parts.len().checked_sub(1)
        {
            return Some((open.msn, last));
        }
        self.done.back().and_then(|s| s.parts.len().checked_sub(1).map(|p| (s.msn, p)))
    }

    /// The number the next part gets: the one a preload hint names.
    pub(crate) fn next_part(&self) -> Option<(u64, usize)> {
        self.open.as_ref().map(|open| (open.msn, open.parts.len()))
    }

    /// Whether segment `msn` (or its part `part`) is complete.
    pub(crate) fn has(&self, msn: u64, part: Option<usize>) -> bool {
        match part {
            None => self.last_segment().is_some_and(|last| last >= msn),
            Some(index) => {
                self.last_segment().is_some_and(|last| last >= msn)
                    || self.last_part().is_some_and(|last| last >= (msn, index))
            }
        }
    }

    /// The number of the segment (complete or being written) that starts at
    /// `ms` milliseconds from the session's zero: DASH's `$Time$`, as the
    /// manifest's timeline writes it.
    pub(crate) fn at_millis(&self, ms: u64) -> Option<u64> {
        let start = |s: &&Segment| u64::try_from(s.start.as_millis()).ok() == Some(ms);
        self.done.iter().chain(self.open.iter()).find(start).map(|s| s.msn)
    }

    /// The first number in the window.
    pub(crate) fn first(&self) -> Option<u64> {
        self.done.front().or(self.open.as_ref()).map(|s| s.msn)
    }
}
