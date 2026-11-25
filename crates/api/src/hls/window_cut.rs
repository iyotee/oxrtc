//! Where a frame cuts a session's [`Window`]: a segment at a keyframe once
//! the one before has lasted its target, a part before a frame that would
//! take it past its target.

use std::time::Duration;

use super::{Cut, MAX_SEGMENT_BYTES, MAX_SEGMENT_DURATION, Segment, Window};

impl Window {
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
        if !lead {
            return Cut::None;
        }
        if at <= part.start {
            // A part a sound frame opened after a close at its target (the
            // picture late on the wire, the sound sent during it ahead of
            // it): a keyframe of the picture still starts what it would
            // have, and takes that sound with it ([`Window::push`]).
            if part.lead_at.is_some() || !(self.video && key) {
                return Cut::None;
            }
            return if self.starts_segment(open, key, at, part.size) { Cut::Segment } else { Cut::Part };
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
}
