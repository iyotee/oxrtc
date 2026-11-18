//! The sound side of a session's sender: what is not sent, and when the rest
//! leaves ([`crate::pace`]).

use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::{Frame, MediaKind};
use str0m::Rtc;
use str0m::net::Transmit;

use super::Sender;
use crate::pace::{Placed, SoundClock};

impl Sender {
    /// Whether `frame` is sound this session sends that entered oxrtc before
    /// it connected: the sound of a session starts at its connection. A
    /// sound track the session does not send (no media line took it) has
    /// nothing held back, and is not counted.
    pub(super) fn stale(&self, track: usize, frame: &Frame) -> bool {
        let sound = self.tracks.get(track).is_some_and(|out| out.info.kind == MediaKind::Audio && out.target.is_some());
        sound && self.connected.is_some_and(|at| frame.received < at)
    }

    /// The clocks of the sound tracks this session sends, for a (new) track
    /// list; what waited for the old one is forgotten with it.
    pub(super) fn set_clocks(&mut self) {
        let origin = tokio::time::Instant::now();
        let (paced, late_max) = (self.playout.pace_sound, self.playout.sound_late_max);
        self.clocks = self
            .tracks
            .iter()
            .map(|out| {
                let sound = out.info.kind == MediaKind::Audio && out.target.is_some();
                (sound && paced).then(|| SoundClock::new(out.info.clock_rate, origin, late_max))
            })
            .collect();
        self.held.clear();
    }

    /// Takes a sound frame of a track sent on its own clock: it waits for its
    /// time, or is counted late. `false`: the track is not one of those, and
    /// the frame is the caller's to write.
    pub(super) fn hold(&mut self, track: usize, frame: &Arc<Frame>) -> bool {
        let Some(clock) = self.clocks.get_mut(track).and_then(Option::as_mut) else { return false };
        match clock.place(frame.pts, frame.received) {
            Placed::Late(_) => self.late_sound = self.late_sound.saturating_add(1),
            Placed::Due(due) => {
                self.sound_hold = self.sound_hold.max(clock.hold());
                self.sound_hold_sum = self.sound_hold_sum.saturating_add(clock.hold());
                self.sound_placed = self.sound_placed.saturating_add(1);
                if self.held.push(track, frame.clone(), due, tokio::time::Instant::now()).is_some() {
                    self.late_sound = self.late_sound.saturating_add(1);
                }
            }
        }
        true
    }

    /// Writes the sound whose time has come, each write followed by a poll
    /// as str0m asks. Returns the last poll's timeout, if any write was made.
    pub(super) fn release(&mut self, rtc: &mut Rtc, transmits: &mut Vec<Transmit>) -> Result<Option<Instant>, String> {
        let mut next = None;
        loop {
            let now = tokio::time::Instant::now();
            let Some(waiting) = self.held.pop_due(now) else { break };
            if waiting.timed {
                self.pace_overrun = self.pace_overrun.max(now.saturating_duration_since(waiting.due));
            }
            let Some(out) = self.tracks.get_mut(waiting.track) else { continue };
            let time = out.time_of(waiting.frame.pts);
            // Dated when it was due: a frame that left late is late in the
            // sender reports too, and the browser sees it as it is.
            self.write(rtc, waiting.track, &waiting.frame, time, waiting.due.into_std());
            next = Some(self.poll(rtc, transmits)?);
        }
        Ok(next)
    }

    /// The mean hold of the sound frames placed so far.
    pub(super) fn sound_hold_mean(&self) -> std::time::Duration {
        let count = u32::try_from(self.sound_placed).unwrap_or(u32::MAX).max(1);
        self.sound_hold_sum / count
    }

    /// When the next waiting sound leaves.
    pub(super) fn sound_due(&self) -> Option<Instant> {
        self.held.next_due().map(tokio::time::Instant::into_std)
    }
}
