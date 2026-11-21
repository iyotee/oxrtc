//! How a subscription's track list is made from its routes, the first one
//! and those after a source's new list, and how frames are mapped onto it.

use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::stream::StreamExt;
use tokio::time::Instant;

use super::{Event, Subscription};
use crate::channel::Channel;
use crate::frame::Frame;
use crate::media::{MediaKind, TrackInfo};
use crate::route::Route;

impl Subscription {
    /// The time between two pictures of the source its video comes from, by
    /// their own dates, once that source has shown it: what a reader that
    /// guesses the frame rate from the pictures themselves can be told
    /// instead (the RTSP server's `a=framerate`, D36). `None` with no video,
    /// or too few pictures seen yet.
    pub fn picture_interval(&self) -> Option<std::time::Duration> {
        self.video_channel().and_then(Channel::picture_interval)
    }

    /// The time between two of those pictures at the rate the source sends
    /// them: the mean of the newest gaps that are no pause, a whole number of
    /// pictures a second when it is near one. What a Motion JPEG track whose
    /// dates sit on no grid is announced at (the RTSP server's
    /// `a=framerate`, D36), where [`Self::picture_interval`] is the median
    /// gap. `None` with no video, or too few pictures seen yet.
    pub fn picture_rate_interval(&self) -> Option<std::time::Duration> {
        self.video_channel().and_then(Channel::picture_rate_interval)
    }

    /// The grid those pictures are dated on, when they are: the greatest
    /// step every gap between two of them is a whole number of (40 ms for a
    /// camera that stamps 25 slots a second, whether it fills each or one in
    /// three). What a reader that guesses the frame rate from the dates
    /// reads, where [`Self::picture_interval`] is the pace the pictures come
    /// at. `None` for pictures dated as they arrive (a webcam over HTTP).
    pub fn picture_grid(&self) -> Option<std::time::Duration> {
        self.video_channel().and_then(Channel::picture_grid)
    }

    /// The channel of the source its video comes from.
    fn video_channel(&self) -> Option<&Channel> {
        self.routes
            .iter()
            .find(|route| route.chosen.iter().any(|(kind, _)| *kind == MediaKind::Video))
            .map(|route| &*route.slot.channel)
    }

    /// Handles the items already waiting, without waiting for more: what a
    /// deadline decides must count the tracks that did arrive.
    pub(super) fn drain_ready(&mut self) {
        while let Some(Some((input, item))) = self.inputs.next().now_or_never() {
            self.handle(input, item);
        }
    }

    /// Gives up the unresolved routes whose source failed an attempt, and
    /// sends the first track list once every other route is resolved. When
    /// nothing is resolved and every route given up reads this server, the
    /// subscription ends at once: waiting would only repeat that answer.
    pub(super) fn give_up_failed(&mut self) {
        self.drop_declined();
        if self.tracks.is_some() {
            return;
        }
        for route in &mut self.routes {
            if !route.ready() && !route.given_up && route.slot.failures() > route.failures_before {
                tracing::debug!(source = %route.slot.spec, "source failed before its tracks; answering without it");
                route.given_up = true;
            }
        }
        if self.routes.iter().any(|r| r.given_up) && self.routes.iter().all(|r| r.ready() || r.given_up) {
            self.flush_partial();
            let hopeless = self.routes.iter().all(|r| r.given_up && r.slot.is_local());
            if self.tracks.is_none() && hopeless {
                self.deadline = None;
            }
        }
        // Waiting past the probe deadline was for a source still starting.
        if self.extended && self.tracks.is_none() && !self.starting() {
            self.deadline = None;
        }
    }

    /// Lets go of the routes that never resolved and whose source was declined
    /// by its peer since they were made (a 4XX: up, and saying no). A given-up
    /// route kept its source's demand for as long as the consumer read, so the
    /// sound transcode of a webcam with no microphone was run again every few
    /// seconds for the whole viewing (owner's PC, 2026-11-17); go2rtc tries such
    /// a source once per consumer. The consumer goes on with its other media;
    /// left with none before its first track list, it ends with no tracks.
    /// A route that resolved is kept whatever its source says later: that is
    /// a source that works and dropped, and it is retried at once.
    pub(super) fn drop_declined(&mut self) {
        let before = self.routes.len();
        self.routes.retain(|route| {
            let declined = !route.ready() && route.slot.failures() > route.failures_before && route.slot.declined();
            if declined {
                tracing::debug!(source = %route.slot.spec, "source declined before its tracks; let go");
            }
            !declined
        });
        if self.routes.len() == before {
            return;
        }
        self.upstream = crate::route::watch_upstream(&self.routes);
        if self.tracks.is_some() {
            return;
        }
        if self.routes.is_empty() {
            if self.reselection.is_none() {
                self.deadline = None;
            }
            return;
        }
        // The routes left may all be resolved: the first list goes out now,
        // not at the deadline (a dropped route is not a given-up one, which
        // is what lets the list go out without waiting).
        self.rebuild(false);
    }

    /// Whether a source of an unresolved route is still starting: not
    /// resolved, and none of its attempts failed since the route was made.
    fn starting(&self) -> bool {
        self.routes.iter().any(|r| !r.ready() && !r.given_up)
    }

    /// At the probe deadline with no track list sent: a source still
    /// starting (FFmpeg probing a slow camera's picture) is waited for until
    /// the hard deadline, once. A source that failed was given up before.
    pub(super) fn extend_for_starting_sources(&mut self) {
        self.give_up_failed();
        if self.tracks.is_some() || self.extended || !self.starting() {
            return;
        }
        self.extended = true;
        if self.hard_deadline > Instant::now() {
            self.deadline = Some(self.hard_deadline);
        }
    }

    pub(super) fn flush_partial(&mut self) {
        if self.tracks.is_none() && self.routes.iter().any(Route::ready) {
            self.rebuild(true);
        }
    }

    /// Recomputes the subscription's track list from the routes, and emits it
    /// when it changed (the first time, only once every route is resolved,
    /// unless `partial` - the probe deadline passed).
    pub(super) fn rebuild(&mut self, partial: bool) {
        if self.tracks.is_none() && !partial && !self.routes.iter().all(Route::ready) {
            return;
        }
        let mut list = Vec::new();
        for route in &mut self.routes {
            route.map = vec![None; route.tracks.as_ref().map_or(0, |t| t.len())];
        }
        for kind in MediaKind::ALL {
            for route in &mut self.routes {
                let Some(&(_, source_track)) = route.chosen.iter().find(|(k, _)| *k == kind) else { continue };
                let Some(info) = route.tracks.as_ref().and_then(|t| t.get(source_track)) else { continue };
                if let Some(slot) = route.map.get_mut(source_track) {
                    *slot = Some(list.len());
                }
                list.push(info.clone());
            }
        }
        if list.is_empty() {
            return;
        }
        let list: Arc<[TrackInfo]> = list.into();
        if self.tracks.as_deref() == Some(&*list) {
            return;
        }
        let first = self.tracks.is_none();
        self.tracks = Some(list.clone());
        self.deadline = None;
        self.pending.push_back(Event::Tracks(list));
        if first {
            while let Some((input, frame)) = self.backlog.pop_front() {
                if let Some(event) = self.route_frame(input, frame) {
                    self.pending.push_back(event);
                }
            }
        }
    }

    pub(super) fn route_frame(&mut self, input: u64, frame: Arc<Frame>) -> Option<Event> {
        let route = self.routes.iter_mut().find(|r| r.id == input)?;
        let source_track = usize::from(frame.track);
        let track = (*route.map.get(source_track)?)?;
        if route.is_video(source_track) && route.need_keyframe {
            if !frame.keyframe {
                return None;
            }
            route.need_keyframe = false;
        }
        Some(Event::Frame { track, frame })
    }
}
