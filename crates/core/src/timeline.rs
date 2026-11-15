//! The hub's media clock: one timeline per track of a source, applied when a
//! frame is published, so that every egress reads the same time.
//!
//! A driver publishes its source's own timestamps, and those start again with
//! every connection: retina counts from each session's first packet, a
//! transcoder from 0. Passed through, a camera reopened under a long-lived
//! reader moved that reader's clock back by the whole previous run, and a
//! restarted sound transcode put the sound hours away from its picture (what
//! openIVR's segmenter detects).
//!
//! On the timeline instead:
//!
//! - each **run** of a source is anchored to the wall clock: its first frame of
//!   a track is placed at the time it was received ([`crate::Frame::received`]),
//!   counted from the hub's epoch, in the track's clock rate. Every track of
//!   every source of a hub counts from the same epoch, so tracks from two
//!   sources (a camera's picture, a transcoder's sound) share one time base;
//! - within a run the source's own steps are kept: they are its only record of
//!   when each frame was taken (a camera that delivers in bursts keeps its
//!   regular timing, a decoder keeps its reordering);
//! - a step back of more than [`MAX_STEP_BACK_SECS`], or forward of more than
//!   [`MAX_LEAD_SECS`] beyond the wall-clock time between two frames, is a
//!   restart inside the run, anchored again the same way;
//! - a time is never more than [`MAX_LEAD_SECS`] ahead of its frame's
//!   arrival: a source whose clock runs fast in steps each under the bound
//!   above is held there, and so is a lead that one run ends with and the
//!   next is anchored after (a transcode restarted on the cached GOP, again
//!   and again, under a long-lived reader);
//! - an anchor is never at or before the latest time a track was given: the
//!   timeline never goes back, even when the previous run ran ahead of the
//!   wall clock;
//! - a frame relayed from another stream of the same hub ([`Origin::Hub`])
//!   already carries this clock, and keeps it.
//!
//! The timeline outlives the source's connections (a reconnection drops the
//! anchors, not the history) and its track list (a new list continues the
//! timeline of the earlier track of the same kind).

use std::time::Duration;

use tokio::time::Instant;

use crate::media::{MediaKind, TrackInfo};

/// A step back larger than this, in one run, is a restart of the source's
/// clock. Smaller ones are a decoder's frame reordering.
pub const MAX_STEP_BACK_SECS: i64 = 1;
/// A step forward larger than the wall-clock time between two frames plus
/// this is a jump of the source's clock, and no time is placed further than
/// this ahead of its frame's arrival. A transcoder working through the cached
/// group of pictures runs ahead of real time by up to a GOP (6 s on the
/// owner's cameras), one frame at a time, never in one step.
pub const MAX_LEAD_SECS: i64 = 10;

/// Where a published frame's timestamp comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The source's own clock.
    Source,
    /// Another stream of the same hub: already on this timeline.
    Hub,
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    source: i64,
    out: i64,
}

#[derive(Debug, Clone)]
struct TrackClock {
    kind: MediaKind,
    rate: u32,
    /// The current run's mapping, until the source restarts.
    anchor: Option<Anchor>,
    /// The source timestamp of the last frame, and when it was received.
    last: Option<(i64, Instant)>,
    /// The latest time given on this track.
    high: Option<i64>,
}

impl TrackClock {
    fn new(kind: MediaKind, rate: u32) -> Self {
        Self { kind, rate, anchor: None, last: None, high: None }
    }

    fn restart(&mut self) {
        self.anchor = None;
        self.last = None;
    }

    /// Whether `pts`, received at `received`, breaks from the run's last frame.
    fn jumped(&self, pts: i64, received: Instant) -> bool {
        let Some((last, at)) = self.last else { return false };
        let rate = i64::from(self.rate);
        let step = pts.saturating_sub(last);
        let elapsed = ticks(received.saturating_duration_since(at), self.rate);
        step < -rate.saturating_mul(MAX_STEP_BACK_SECS)
            || step > elapsed.saturating_add(rate.saturating_mul(MAX_LEAD_SECS))
    }
}

/// The timelines of one source's tracks.
#[derive(Debug, Default)]
pub(crate) struct Timelines {
    tracks: Vec<TrackClock>,
}

impl Timelines {
    /// The source starts a new run: each track is anchored again at its next frame.
    pub(crate) fn restart(&mut self) {
        self.tracks.iter_mut().for_each(TrackClock::restart);
    }

    /// The source's new track list. Each track continues the timeline of the
    /// first earlier track of its kind not yet taken; at an unchanged clock
    /// rate it keeps its anchor too (a list published again inside a run,
    /// for parameter sets found in band, moves nothing).
    pub(crate) fn set_tracks(&mut self, tracks: &[TrackInfo]) {
        let mut old = std::mem::take(&mut self.tracks);
        self.tracks = tracks
            .iter()
            .map(|info| {
                let rate = info.clock_rate.max(1);
                match old.iter().position(|clock| clock.kind == info.kind) {
                    Some(at) => {
                        let mut clock = old.remove(at);
                        if clock.rate != rate {
                            clock.high = clock.high.map(|high| rescale(high, clock.rate, rate));
                            clock.restart();
                            clock.rate = rate;
                        }
                        clock
                    }
                    None => TrackClock::new(info.kind, rate),
                }
            })
            .collect();
    }

    /// The time on the timeline of a frame of `track` with the source
    /// timestamp `pts`, received at `received`. Never negative.
    pub(crate) fn map(&mut self, track: usize, pts: i64, received: Instant, epoch: Instant, origin: Origin) -> i64 {
        let Some(clock) = self.tracks.get_mut(track) else { return pts.max(0) };
        let anchor = match clock.anchor {
            Some(anchor) if !clock.jumped(pts, received) => anchor,
            _ => {
                let target = match origin {
                    Origin::Source => ticks(received.saturating_duration_since(epoch), clock.rate),
                    Origin::Hub => pts,
                };
                let step = i64::from(clock.rate / 25).max(1);
                let out = clock.high.map_or(target, |high| target.max(high.saturating_add(step)));
                let anchor = Anchor { source: pts, out };
                clock.anchor = Some(anchor);
                anchor
            }
        };
        let mut out = anchor.out.saturating_add(pts.saturating_sub(anchor.source)).max(0);
        // Every step under the bound above, the source's clock can still run
        // away from the wall clock, and a run that ends ahead hands its lead
        // to the next one (anchored after it): the time is held at most
        // MAX_LEAD_SECS ahead of the frame's arrival, and continues from
        // there. Never back: what came before was held as well.
        let ceiling = ticks(received.saturating_duration_since(epoch), clock.rate)
            .saturating_add(i64::from(clock.rate).saturating_mul(MAX_LEAD_SECS));
        if out > ceiling {
            out = clock.high.map_or(ceiling, |high| ceiling.max(high.saturating_add(1)));
            clock.anchor = Some(Anchor { source: pts, out });
        }
        clock.high = Some(clock.high.map_or(out, |high| high.max(out)));
        clock.last = Some((pts, received));
        out
    }
}

/// A duration in ticks of a `rate` Hz clock.
pub fn ticks(duration: Duration, rate: u32) -> i64 {
    i64::try_from(duration.as_nanos().saturating_mul(u128::from(rate)) / 1_000_000_000).unwrap_or(i64::MAX)
}

/// `value` ticks of a `from` Hz clock in a `to` Hz one, rounded up.
fn rescale(value: i64, from: u32, to: u32) -> i64 {
    let from = i128::from(from.max(1));
    let scaled = (i128::from(value) * i128::from(to) + from - 1).div_euclid(from);
    i64::try_from(scaled).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod tests;
