//! The sound side of a session's sender: what is not sent, when the rest
//! leaves ([`crate::pace`]), and how the sound is dated against the picture.

use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::{Frame, MediaKind};
use str0m::Rtc;
use str0m::net::Transmit;

use super::Sender;
use crate::pace::{Placed, SoundClock};
use crate::{MAX_SOUND_LAG, Playout};

/// How much earlier than it arrived a session's picture is dated, for
/// [`Playout::sound_lag`] (D30, roadmap W1c): nothing with lip sync off, or
/// when the session sends no sound (`sound`), and never more than
/// [`MAX_SOUND_LAG`].
pub(super) fn picture_lead(playout: Playout, sound: bool) -> std::time::Duration {
    if playout.lip_sync && sound { playout.sound_lag.min(MAX_SOUND_LAG) } else { std::time::Duration::ZERO }
}

/// The wall clock str0m is given for a frame of `kind` that arrived at
/// `wall`: what its track's sender reports then say of its RTP time.
///
/// A browser synchronises a session's two tracks from those reports, so only
/// the difference between the two datings exists for it. The sound dated
/// `lead` later than it arrived is written as the picture dated `lead`
/// earlier, which is the same difference: str0m takes no date in the future
/// (its sender report then carries RTP time 0 until the clock passes it), and
/// a sound frame dated 20 ms ahead would be in the future for most of the 20
/// to 40 ms until the next one. A picture whose date cannot be moved (a clock
/// younger than `lead`) keeps its arrival.
pub(super) fn dated(kind: MediaKind, wall: Instant, lead: std::time::Duration) -> Instant {
    match kind {
        MediaKind::Video => wall.checked_sub(lead).unwrap_or(wall),
        MediaKind::Audio => wall,
    }
}

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
    /// list, when the sound is paced or bounded (the bound needs the clock to
    /// know how late a frame is); what waited for the old one is forgotten
    /// with it. And how the picture is dated against that sound.
    pub(super) fn set_clocks(&mut self) {
        let sends_sound = self.tracks.iter().any(|out| out.info.kind == MediaKind::Audio && out.target.is_some());
        self.picture_lead = picture_lead(self.playout, sends_sound);
        let origin = tokio::time::Instant::now();
        let (paced, late_max) = (self.playout.pace_sound, self.playout.sound_late_max);
        self.clocks = self
            .tracks
            .iter()
            .map(|out| {
                let sound = out.info.kind == MediaKind::Audio && out.target.is_some();
                (sound && (paced || late_max.is_some())).then(|| SoundClock::new(out.info.clock_rate, origin, late_max))
            })
            .collect();
        self.held.clear();
    }

    /// Takes a sound frame of a track that has a clock: it is counted late
    /// and not sent, or it waits for its time (paced). `false`: the frame is
    /// the caller's to write, now - a track with no clock, or a frame on time
    /// when the sound is bounded and not paced.
    pub(super) fn hold(&mut self, track: usize, frame: &Arc<Frame>) -> bool {
        let Some(clock) = self.clocks.get_mut(track).and_then(Option::as_mut) else { return false };
        match clock.place(frame.pts, frame.received) {
            Placed::Late(_) => self.late_sound = self.late_sound.saturating_add(1),
            Placed::Due(_) if !self.playout.pace_sound => return false,
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
