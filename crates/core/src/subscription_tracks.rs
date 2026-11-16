//! How a subscription's track list is made from its routes, the first one
//! and those after a source's new list, and how frames are mapped onto it.

use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::stream::StreamExt;
use tokio::time::Instant;

use super::{Event, Subscription};
use crate::frame::Frame;
use crate::media::{MediaKind, TrackInfo};
use crate::route::Route;

impl Subscription {
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
