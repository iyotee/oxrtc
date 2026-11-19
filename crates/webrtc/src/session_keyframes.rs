//! The picture side of a session's sender: a viewer's keyframe request, and
//! the cached group of pictures sent again ([`crate::replay`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use oxrtc_core::{Frame, MediaKind};
use str0m::Rtc;
use str0m::media::KeyframeRequest;
use str0m::net::Transmit;
use tracing::debug;

use super::Sender;
use crate::replay::{Answer, stamps};

impl Sender {
    /// Sends the cached group of pictures again ([`crate::replay`]), each
    /// write followed by a poll as str0m asks. Returns the last poll's
    /// timeout, if any write was made, and whether a group went whole.
    pub(super) fn replay(
        &mut self,
        rtc: &mut Rtc,
        frames: Vec<(usize, Arc<Frame>)>,
        transmits: &mut Vec<Transmit>,
    ) -> Result<(Option<Instant>, bool), String> {
        let (mut next, mut resumed) = (None, false);
        let mut groups: Vec<(usize, Vec<Arc<Frame>>)> = Vec::new();
        for (track, frame) in frames {
            match groups.iter_mut().find(|(t, _)| *t == track) {
                Some((_, group)) => group.push(frame),
                None => groups.push((track, vec![frame])),
            }
        }
        for (track, group) in groups {
            let Some(times) = self.replay_times(track, &group) else { continue };
            let mut whole = true;
            for (frame, (time, wall)) in group.iter().zip(times) {
                whole &= self.write(rtc, track, frame, time, wall);
                self.replays.frames = self.replays.frames.saturating_add(1);
                next = Some(self.poll(rtc, transmits)?);
            }
            let end = group.last().map_or(0, |f| f.pts);
            if let Some(out) = self.tracks.get_mut(track) {
                // A write refused mid-way breaks the chain again: video
                // then waits for a keyframe, as after any loss.
                if whole {
                    out.replayed(end);
                    resumed = true;
                }
            }
            debug!(track, frames = group.len(), whole, "keyframe request answered from the cached group of pictures");
        }
        Ok((next, resumed))
    }

    /// The RTP times and wall clocks a replayed group is written with: its
    /// own while the track sent nothing yet, else times of its own after
    /// the last one sent ([`stamps`]). `None`: not replayed.
    fn replay_times(&self, track: usize, group: &[Arc<Frame>]) -> Option<Vec<(str0m::media::MediaTime, Instant)>> {
        let out = self.tracks.get(track).filter(|out| out.target.is_some())?;
        if !group.first().is_some_and(|f| f.keyframe) {
            return None;
        }
        let Some((last, wall)) = out.last_sent() else {
            return Some(group.iter().map(|f| (out.rtp_time(f.pts), f.received.into_std())).collect());
        };
        let end = group.last().map_or(0, |f| u64::try_from(f.pts).unwrap_or(0));
        let Some(times) = stamps(last, end, out.interval(), group.len()) else {
            debug!(track, frames = group.len(), "keyframe request not answered: the cached group does not fit");
            return None;
        };
        Some(times.into_iter().map(|t| (out.media_time(t), wall)).collect())
    }

    /// A viewer's keyframe request, for a picture this session sends: a
    /// replay, or the camera's next keyframe ([`crate::replay::Replays::answer`]),
    /// the picture stopped until it - what would be sent before it cannot
    /// be decoded, and a browser that is behind has no time to spare on it.
    pub(super) fn keyframe_request(&mut self, request: &KeyframeRequest, now: Instant) {
        let video = self
            .tracks
            .iter()
            .any(|t| t.info.kind == MediaKind::Video && t.target.is_some_and(|(mid, _)| mid == request.mid));
        debug!(kind = ?request.kind, video, "keyframe request");
        if !video {
            return;
        }
        match self.replays.answer(now, self.next_keyframe()) {
            Answer::Replay => self.replays.request(),
            Answer::Camera => {
                self.replays.decline();
                for out in self.tracks.iter_mut().filter(|t| t.info.kind == MediaKind::Video) {
                    out.lost();
                }
            }
        }
    }

    /// How soon the camera's next keyframe is due on the pictures this
    /// session sends; `None` while its interval is not known.
    fn next_keyframe(&self) -> Option<Duration> {
        self.tracks
            .iter()
            .filter(|t| t.info.kind == MediaKind::Video && t.target.is_some())
            .filter_map(|t| {
                let ticks = t.keys.until_next()?;
                Some(Duration::from_micros(ticks.saturating_mul(1_000_000) / u64::from(t.info.clock_rate.max(1))))
            })
            .min()
    }

    /// The subscription lost frames for this session ([`oxrtc_core::Event::Discontinuity`]):
    /// a picture it sends resumes from the cached group of pictures.
    pub(super) fn discontinuity(&mut self) {
        if self.tracks.iter().any(|t| t.info.kind == MediaKind::Video && t.target.is_some()) {
            self.replays.resume();
        }
    }
}
