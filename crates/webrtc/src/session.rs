//! One WebRTC session: drives its `Rtc` and forwards the subscription's
//! frames to it.

use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::{Event, Frame, MediaKind, Subscription, TrackInfo};
use str0m::media::KeyframeRequest;
use str0m::net::Transmit;
use str0m::{Event as RtcEvent, IceConnectionState, Input, Output as RtcOutput, Rtc, RtcError};
use tracing::{debug, info, warn};

use crate::payload::{Output, TrackOut, choose};
use crate::registry::Session;
use crate::replay::{Replays, stamps};
use crate::server::Shared;
use crate::socket::send;

/// Outputs drained when a session closes (RTCP BYE, DTLS close_notify).
const CLOSE_DRAIN: usize = 64;

/// The `Rtc` of a session, and when its peer was last heard from.
pub(crate) struct Peer {
    pub(crate) rtc: Rtc,
    pub(crate) last_rx: Instant,
}

/// The sending side of a session. It reads the subscription only once
/// connected: until then frames wait in the subscription's queue, under its
/// live budget, and the cached group of pictures with them.
struct Sender {
    outputs: Vec<Output>,
    tracks: Vec<TrackOut>,
    connected: bool,
    replays: Replays,
    sent: u64,
    dropped: u64,
    full: u64,
    /// Queued frames a replay had sent already.
    repeated: u64,
}

impl Sender {
    fn new(outputs: Vec<Output>, tracks: &[TrackInfo]) -> Self {
        let mut sender = Self {
            outputs,
            tracks: Vec::new(),
            connected: false,
            replays: Replays::default(),
            sent: 0,
            dropped: 0,
            full: 0,
            repeated: 0,
        };
        sender.set_tracks(tracks);
        sender
    }

    /// Maps a (new) track list onto the negotiated outputs. Whether any
    /// track can be sent.
    fn set_tracks(&mut self, tracks: &[TrackInfo]) -> bool {
        // A new list may follow a reconnected source; its frames' times
        // continue all the same (the hub's timeline), nothing to carry over.
        self.tracks = tracks
            .iter()
            .map(|info| {
                let target = choose(&self.outputs, info);
                if target.is_none() {
                    debug!(codec = info.codec.as_str(), "track not sent: no negotiated payload type");
                }
                TrackOut::new(info.clone(), target)
            })
            .collect();
        self.tracks.iter().any(|t| t.target.is_some())
    }

    /// A frame from the subscription.
    fn frame(&mut self, rtc: &mut Rtc, track: usize, frame: &Frame) {
        let Some(out) = self.tracks.get_mut(track) else { return };
        if out.repeats(frame.pts) {
            self.repeated = self.repeated.saturating_add(1);
            return;
        }
        if !out.admit(frame.keyframe) {
            return;
        }
        let time = out.time_of(frame.pts);
        self.write(rtc, track, frame, time, frame.received.into_std());
    }

    /// Writes `frame` at RTP `time`, dated `wall`. Whether it was taken.
    fn write(
        &mut self,
        rtc: &mut Rtc,
        track: usize,
        frame: &Frame,
        time: str0m::media::MediaTime,
        wall: Instant,
    ) -> bool {
        let Some(out) = self.tracks.get_mut(track) else { return false };
        let Some((mid, pt)) = out.target else { return false };
        let data = out.payload(frame);
        let Some(writer) = rtc.writer(mid) else {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        };
        match writer.write(pt, wall, time, data) {
            Ok(()) => {
                self.sent = self.sent.saturating_add(1);
                out.sent(time.numer(), wall);
                true
            }
            Err(error) => {
                if matches!(error, RtcError::WriteWithoutPoll) {
                    self.full = self.full.saturating_add(1);
                } else {
                    debug!(%error, "frame not written");
                }
                self.dropped = self.dropped.saturating_add(1);
                out.lost();
                false
            }
        }
    }

    /// Sends the cached group of pictures again ([`crate::replay`]), each
    /// write followed by a poll as str0m asks. Returns the last poll's
    /// timeout, if any write was made, and whether a group went whole.
    fn replay(
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

    /// A viewer's keyframe request: for a picture this session sends, a
    /// replay is asked for.
    fn keyframe_request(&mut self, request: &KeyframeRequest) {
        let video = self
            .tracks
            .iter()
            .any(|t| t.info.kind == MediaKind::Video && t.target.is_some_and(|(mid, _)| mid == request.mid));
        debug!(kind = ?request.kind, video, "keyframe request");
        if video {
            self.replays.request();
        }
    }

    /// The subscription lost frames for this session ([`Event::Discontinuity`]):
    /// a picture it sends resumes from the cached group of pictures.
    fn discontinuity(&mut self) {
        if self.tracks.iter().any(|t| t.info.kind == MediaKind::Video && t.target.is_some()) {
            self.replays.resume();
        }
    }

    /// Advances the `Rtc` to now and collects what it has to send. Returns
    /// when it next needs a timeout, or why the session is over.
    fn drive(&mut self, rtc: &mut Rtc, transmits: &mut Vec<Transmit>) -> Result<Instant, String> {
        rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e| e.to_string())?;
        let next = self.poll(rtc, transmits)?;
        if !rtc.is_alive() {
            return Err("closed".into());
        }
        Ok(next)
    }

    fn poll(&mut self, rtc: &mut Rtc, transmits: &mut Vec<Transmit>) -> Result<Instant, String> {
        loop {
            match rtc.poll_output().map_err(|e| e.to_string())? {
                RtcOutput::Timeout(at) => return Ok(at),
                RtcOutput::Transmit(transmit) => transmits.push(transmit),
                RtcOutput::Event(event) => match event {
                    RtcEvent::Connected => {
                        debug!("ice and dtls connected");
                        self.connected = true;
                    }
                    RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Err("ice disconnected".into());
                    }
                    RtcEvent::Closed => return Err("closed by the peer".into()),
                    RtcEvent::KeyframeRequest(request) => self.keyframe_request(&request),
                    _ => {}
                },
            }
        }
    }
}

/// Removes the session from the registry however its task ends.
struct Unregister(Arc<Shared>, u64);

impl Drop for Unregister {
    fn drop(&mut self) {
        self.0.unregister(self.1);
    }
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    session: Arc<Session>,
    mut subscription: Subscription,
    outputs: Vec<Output>,
    tracks: Arc<[TrackInfo]>,
) {
    let _unregister = Unregister(shared.clone(), session.id);
    let mut sender = Sender::new(outputs, &tracks);
    let mut transmits = Vec::new();
    let idle = shared.config.session_idle_timeout;
    let reason: String = loop {
        let (step, last_rx) = {
            let mut peer = session.peer();
            let Peer { rtc, last_rx } = &mut *peer;
            let mut step = sender.drive(rtc, &mut transmits);
            if let Ok(next) = step
                && sender.replays.take(Instant::now())
            {
                step = sender.replay(rtc, subscription.cached_video(), &mut transmits).map(|(last, resumed)| {
                    if resumed {
                        subscription.video_resumed();
                    }
                    last.map_or(next, |at| at.min(next))
                });
            }
            (step, *last_rx)
        };
        send(&shared, &session.candidates, &mut transmits).await;
        let next = match step {
            Ok(next) => next,
            Err(reason) => break reason,
        };
        let idle_at = last_rx + idle;
        if Instant::now() >= idle_at {
            break "idle timeout".into();
        }
        let mut wake_at = next.min(idle_at);
        if let Some(replay_at) = sender.replays.wake_at() {
            wake_at = wake_at.min(replay_at);
        }
        let wake_at = tokio::time::Instant::from_std(wake_at);
        tokio::select! {
            biased;
            () = session.cancel.cancelled() => break "cancelled".into(),
            () = session.wake.notified() => {}
            () = tokio::time::sleep_until(wake_at) => {}
            event = subscription.recv(), if sender.connected => match event {
                Err(end) => break end.to_string(),
                Ok(Event::Frame { track, frame }) => {
                    let mut peer = session.peer();
                    sender.frame(&mut peer.rtc, track, &frame);
                }
                Ok(Event::Tracks(list)) => {
                    if !sender.set_tracks(&list) {
                        break "the stream's new tracks cannot be sent".into();
                    }
                }
                // The subscription waits for a keyframe, which can be a whole
                // group of pictures away (or never in the queue, for a session
                // that connected late): the cached one is sent instead.
                Ok(Event::Discontinuity) => sender.discontinuity(),
            },
        }
    };
    drop(subscription);
    {
        let mut peer = session.peer();
        let _ = peer.rtc.close();
        for _ in 0..CLOSE_DRAIN {
            match peer.rtc.poll_output() {
                Ok(RtcOutput::Transmit(transmit)) => transmits.push(transmit),
                Ok(RtcOutput::Event(_)) => {}
                Ok(RtcOutput::Timeout(_)) | Err(_) => break,
            }
        }
    }
    send(&shared, &session.candidates, &mut transmits).await;
    let video = sender.tracks.iter().any(|t| t.info.kind == MediaKind::Video);
    if sender.full > 0 {
        warn!(session = session.id, full = sender.full, "str0m's send queue was full; video dropped to keyframes");
    }
    info!(
        session = session.id,
        %reason,
        sent = sender.sent,
        dropped = sender.dropped,
        video,
        keyframe_requests = sender.replays.requested,
        replays = sender.replays.sent,
        repeated = sender.repeated,
        "webrtc session ended"
    );
}
