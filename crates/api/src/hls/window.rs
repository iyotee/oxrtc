//! An HLS session's media: the frames of its feed cut into parts and
//! segments, and the window of segments its playlists list (no I/O, no
//! clock: the times are the feed's, from the session's zero).
//!
//! A segment starts at a keyframe of the video (any frame of a stream with
//! no video) once the one before has lasted [`Settings::segment`]; a part
//! starts at each keyframe and before a frame that would take it past its
//! target ([`Window::part_target`]: [`Settings::part`], or one frame when
//! frames come further apart), so that a part never outlasts it (a frame
//! every 40 ms at 25 pictures a second: parts of 80 ms). A part or segment is
//! closed where the frame that starts the next one begins - or, after a gap
//! (a source that stalled with its clock going on), where its own media
//! ends: the gap is in no part, and a segment lasts its parts. A source that
//! stalls leaves the part being written open with no frame to close it: the
//! session closes it at its target on the wall clock ([`Window::seal`]), so
//! that `PART-TARGET` and `PART-HOLD-BACK` stay put whatever the source does
//! (F6P). A segment that never meets a keyframe is closed at
//! [`MAX_SEGMENT_DURATION`] or [`MAX_SEGMENT_BYTES`] all the same, the next
//! one marked as not independent.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use bytes::Bytes;

pub(crate) use super::part::Part;
use super::part::{OpenPart, segment_of, tagged};
use crate::fmp4::gaps::Steps;

/// The length a segment reaches before the next keyframe closes it
/// (MediaMTX's `hlsSegmentDuration`).
pub const SEGMENT_TARGET: Duration = Duration::from_secs(1);
/// A part's target: half MediaMTX's `hlsPartDuration` (200 ms), which
/// with `PART-HOLD-BACK` at twice the target played hls.js 0.5 s behind
/// the camera, go2rtc's fMP4 a hair ahead; 100 ms plays it 0.29 s behind
/// (D47, the F6 review's four passes).
pub const PART_TARGET: Duration = Duration::from_millis(100);
/// Segments a session keeps at most (MediaMTX's `hlsSegmentCount`).
pub const MAX_SEGMENTS: usize = 7;
/// Segments a session keeps whatever its memory share: the playlist a
/// player reads must list more than the one it plays.
pub const MIN_SEGMENTS: usize = 2;
/// A segment longer than this is closed without a keyframe.
pub const MAX_SEGMENT_DURATION: Duration = Duration::from_secs(30);
/// A segment larger than this is closed without a keyframe (MediaMTX's
/// `hlsSegmentMaxSize` is 50 MB; oxrtc's output budget is smaller).
pub const MAX_SEGMENT_BYTES: usize = 32 * 1024 * 1024;

/// How a session cuts its media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// [`SEGMENT_TARGET`].
    pub segment: Duration,
    /// [`PART_TARGET`].
    pub part: Duration,
    /// [`MAX_SEGMENTS`].
    pub max_segments: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self { segment: SEGMENT_TARGET, part: PART_TARGET, max_segments: MAX_SEGMENTS }
    }
}

/// A segment: complete, or the one being written.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    /// Its media sequence number.
    pub msn: u64,
    /// Its start, from the session's zero.
    pub start: Duration,
    /// Its length, once complete.
    pub duration: Duration,
    /// The wall-clock time of its first frame (`EXT-X-PROGRAM-DATE-TIME`).
    pub wall: SystemTime,
    /// The init segment it is read with.
    pub init: u64,
    /// Whether it follows another track list (`EXT-X-DISCONTINUITY`).
    pub discontinuity: bool,
    /// Whether it starts at a keyframe.
    pub independent: bool,
    /// Its complete parts.
    pub parts: Vec<Part>,
    size: usize,
}

/// What a frame does to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// It starts a segment (and a part).
    Segment,
    /// It starts a part.
    Part,
    /// It goes into the part being written.
    None,
}

/// A session's segments.
#[derive(Debug)]
pub(crate) struct Window {
    settings: Settings,
    /// Whether the feed has a video track, whose keyframes cut.
    video: bool,
    done: VecDeque<Segment>,
    open: Option<Segment>,
    part: Option<OpenPart>,
    next_msn: u64,
    init: u64,
    discontinuity: bool,
    /// The discontinuities that left the window (`EXT-X-DISCONTINUITY-SEQUENCE`).
    discontinuities: u64,
    /// The init segments (fMP4) of the segments in the window and the
    /// current one, by number: a playlist's `EXT-X-MAP` names each.
    inits: VecDeque<(u64, Bytes)>,
    /// The lead track's step between frames.
    steps: Steps,
    /// The part target, fixed once the first segment is complete.
    target: Option<Duration>,
    /// Where the part closed at its target ended, until the next opens.
    sealed: Option<Duration>,
    longest: Duration,
    size: usize,
}

impl Window {
    pub(crate) fn new(settings: Settings) -> Self {
        Self {
            settings,
            video: false,
            done: VecDeque::new(),
            open: None,
            part: None,
            next_msn: 0,
            init: 0,
            discontinuity: false,
            discontinuities: 0,
            inits: VecDeque::new(),
            steps: Steps::default(),
            target: None,
            sealed: None,
            longest: Duration::ZERO,
            size: 0,
        }
    }

    /// The settings.
    pub(crate) fn settings(&self) -> Settings {
        self.settings
    }

    /// A new init segment (a new track list; `bytes`, fMP4's, kept while a
    /// segment of the window is read with it): what is being written is
    /// closed where its last frame was, and the next segment follows a
    /// discontinuity when anything came before it.
    pub(crate) fn new_init(&mut self, video: bool, bytes: Option<Bytes>) {
        self.close_open(None);
        self.discontinuity = !self.done.is_empty();
        self.init += 1;
        self.video = video;
        if let Some(bytes) = bytes {
            self.size += bytes.len();
            self.inits.push_back((self.init, bytes));
        }
        self.forget_inits();
    }

    /// The current init segment's number.
    pub(crate) fn init(&self) -> u64 {
        self.init
    }

    /// Init segment `number`, while it is the current one or a segment of
    /// the window is read with it.
    pub(crate) fn init_segment(&self, number: u64) -> Option<Bytes> {
        self.inits.iter().find(|(n, _)| *n == number).map(|(_, bytes)| bytes.clone())
    }

    /// Lets go of the init segments no segment of the window is read with.
    fn forget_inits(&mut self) {
        let (current, done, open) = (self.init, &self.done, &self.open);
        let size = &mut self.size;
        self.inits.retain(|(n, bytes)| {
            let kept = *n == current || done.iter().chain(open.iter()).any(|s| s.init == *n);
            if !kept {
                *size = size.saturating_sub(bytes.len());
            }
            kept
        });
    }

    /// The discontinuities that left the window: the playlist's
    /// `EXT-X-DISCONTINUITY-SEQUENCE` (RFC 8216 4.3.3.3), so that a player
    /// counts the segments after one as it did while it was listed.
    pub(crate) fn discontinuities(&self) -> u64 {
        self.discontinuities
    }

    /// Closes the segment being written (and its part, as [`Window::close_part`]).
    fn close_open(&mut self, next: Option<Duration>) {
        self.close_part(next);
        let Some(mut open) = self.open.take() else { return };
        // What its parts hold: a gap of a stalled source is in none of them.
        open.duration = open.parts.iter().map(|p| p.duration).sum();
        self.longest = self.longest.max(open.duration);
        if open.parts.is_empty() {
            return;
        }
        self.done.push_back(open);
        // The part target is fixed from the first segment on: `PART-TARGET`
        // does not change under a player.
        if self.target.is_none() {
            self.target = Some(self.part_target());
        }
    }

    /// Closes the part being written where the frame at `next` begins - or
    /// where its own media ends, when `next` comes after a gap (a stalled
    /// source) or is not given.
    fn close_part(&mut self, next: Option<Duration>) {
        let step = self.steps.step();
        let (Some(open), Some(part)) = (self.open.as_mut(), self.part.take()) else { return };
        let slack = if part.exact { Duration::from_millis(2) } else { step / 2 };
        let media_end = part.media_end(step);
        let end = match next {
            Some(next) if next <= media_end + slack => next,
            _ => media_end,
        };
        open.size += part.size;
        let start = part.start;
        segment_of(open, part, end.max(start));
    }

    /// Closes the part being written at its target, whatever the source
    /// does: the session calls it once that target has passed on the wall
    /// clock with no frame to close it (a stalled source). The part ends
    /// where its media ends, never past its target. Whether a part was closed.
    pub(crate) fn seal(&mut self) -> bool {
        let (target, step) = (self.part_target(), self.steps.step());
        let (Some(open), Some(part)) = (self.open.as_mut(), self.part.take()) else { return false };
        let end = part.media_end(step).min(part.start + target).max(part.start);
        open.size += part.size;
        segment_of(open, part, end);
        self.sealed = Some(end);
        true
    }

    /// The part target: [`Settings::part`], or the lead track's step between
    /// frames when that is longer (a camera of 5 pictures a second has parts
    /// of one picture, 200 ms) - fixed once the first segment is complete.
    pub(crate) fn part_target(&self) -> Duration {
        self.target.unwrap_or_else(|| self.settings.part.max(self.steps.step()))
    }

    /// The lead track's step between frames.
    pub(crate) fn step(&self) -> Duration {
        self.steps.step()
    }

    /// Whether a part is being written (a source that stalls leaves one open
    /// until [`Window::seal`]).
    pub(crate) fn part_open(&self) -> bool {
        self.part.is_some()
    }

    fn starts_segment(&self, open: &Segment, key: bool, at: Duration, part_size: usize) -> bool {
        let long = at.saturating_sub(open.start);
        // A segment starts at a keyframe of the video, or at any frame of a
        // stream with none.
        ((key || !self.video) && long >= self.settings.segment)
            || long >= MAX_SEGMENT_DURATION
            || open.size + part_size > MAX_SEGMENT_BYTES
    }

    /// What a frame at `at` (its media ending at `end` when the container
    /// says it) does, `key` when it is a keyframe of the video; only a frame
    /// of the lead track (the video, else any) cuts: the times of two tracks
    /// are never exactly in step, and a sound frame a few milliseconds ahead
    /// of the picture must not hold a keyframe back. After a part closed at
    /// its target, any frame starts the next one.
    pub(crate) fn cut(&self, lead: bool, key: bool, at: Duration, end: Option<Duration>) -> Cut {
        let Some(open) = &self.open else {
            return if lead { Cut::Segment } else { Cut::None };
        };
        let Some(part) = &self.part else {
            return if lead && self.starts_segment(open, key, at, 0) { Cut::Segment } else { Cut::Part };
        };
        if !lead || at <= part.start {
            return Cut::None;
        }
        if self.starts_segment(open, key, at, part.size) {
            return Cut::Segment;
        }
        let frame_end = end.unwrap_or(at + self.steps.step());
        if (self.video && key) || frame_end.saturating_sub(part.start) > self.part_target() {
            return Cut::Part;
        }
        Cut::None
    }

    /// Adds a frame's bytes (`chunks`: the tables a segment starts with
    /// first, when it has them), at `at` and ending at `end` when the
    /// container says where (fMP4's sample), cut as [`Window::cut`] says; a
    /// frame that comes before any segment and cannot start one is dropped.
    pub(crate) fn push(
        &mut self,
        chunks: Vec<Bytes>,
        lead: bool,
        key: bool,
        at: Duration,
        end: Option<Duration>,
        wall: SystemTime,
    ) {
        let cut = self.cut(lead, key, at, end);
        if lead {
            self.steps.add(at);
        }
        let size: usize = chunks.iter().map(Bytes::len).sum();
        let independent = key || !self.video;
        let other_end = end.unwrap_or(at);
        // A part opened after one closed at its target starts where that one
        // ended at the earliest: sound a little behind the picture must not
        // reach back into it.
        let start = self.sealed.take().map_or(at, |sealed| sealed.max(at));
        let opened = |chunks| OpenPart {
            start,
            end: if lead { start } else { other_end.max(start) },
            lead_at: lead.then_some(at),
            lead_end: if lead { end } else { None },
            exact: end.is_some(),
            independent,
            chunks: tagged(chunks, lead),
            size,
        };
        match cut {
            Cut::Segment => {
                self.close_open(Some(at));
                let discontinuity = std::mem::take(&mut self.discontinuity);
                self.open = Some(Segment {
                    msn: self.next_msn,
                    start,
                    duration: Duration::ZERO,
                    wall,
                    init: self.init,
                    discontinuity,
                    independent,
                    parts: Vec::new(),
                    size: 0,
                });
                self.next_msn += 1;
                self.part = Some(opened(chunks));
            }
            Cut::Part => {
                self.close_part(Some(at));
                self.part = Some(opened(chunks));
            }
            Cut::None => {
                if let Some(part) = self.part.as_mut() {
                    part.chunks.extend(tagged(chunks, lead));
                    part.size += size;
                    if lead {
                        part.lead_at = Some(part.lead_at.map_or(at, |a| a.max(at)));
                        part.lead_end = end.max(part.lead_end);
                    } else {
                        part.end = part.end.max(other_end);
                    }
                }
            }
        }
        // A frame with nowhere to go (sound before the first picture) is
        // not held.
        if cut != Cut::None || self.part.is_some() {
            self.size += size;
        }
    }

    /// Drops the oldest segments beyond [`Settings::max_segments`] or
    /// `max_bytes`, never below [`MIN_SEGMENTS`].
    pub(crate) fn evict(&mut self, max_bytes: usize) {
        while self.done.len() > MIN_SEGMENTS && (self.done.len() > self.settings.max_segments || self.size > max_bytes)
        {
            if let Some(old) = self.done.pop_front() {
                let held: usize = old.parts.iter().map(|p| p.bytes.len()).sum();
                self.size = self.size.saturating_sub(held);
                // Its discontinuity leaves with it; the first segment
                // listed now counts it.
                self.discontinuities += u64::from(old.discontinuity);
            }
        }
        self.forget_inits();
    }
}

#[path = "window_read.rs"]
mod read;

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
