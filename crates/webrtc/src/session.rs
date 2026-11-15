//! One WebRTC session: drives its `Rtc` and forwards the subscription's
//! frames to it.

use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::{Event, Frame, MediaKind, Subscription, TrackInfo};
use str0m::net::Transmit;
use str0m::{Event as RtcEvent, IceConnectionState, Input, Output as RtcOutput, Rtc, RtcError};
use tracing::{debug, info, warn};

use crate::payload::{Output, Pending, TrackOut, choose};
use crate::registry::Session;
use crate::server::Shared;
use crate::socket::send;

/// Outputs drained when a session closes (RTCP BYE, DTLS close_notify).
const CLOSE_DRAIN: usize = 64;

/// The `Rtc` of a session, and when its peer was last heard from.
pub(crate) struct Peer {
    pub(crate) rtc: Rtc,
    pub(crate) last_rx: Instant,
}

/// The sending side of a session.
struct Sender {
    outputs: Vec<Output>,
    tracks: Vec<TrackOut>,
    pending: Pending,
    connected: bool,
    sent: u64,
    dropped: u64,
    full: u64,
}

impl Sender {
    fn new(outputs: Vec<Output>, tracks: &[TrackInfo]) -> Self {
        let mut sender = Self {
            outputs,
            tracks: Vec::new(),
            pending: Pending::default(),
            connected: false,
            sent: 0,
            dropped: 0,
            full: 0,
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
        self.pending.clear();
        self.tracks.iter().any(|t| t.target.is_some())
    }

    /// A frame from the subscription.
    fn frame(&mut self, rtc: &mut Rtc, track: usize, frame: Arc<Frame>) {
        let Some(out) = self.tracks.get_mut(track) else { return };
        if !out.admit(frame.keyframe) {
            return;
        }
        if !self.connected {
            if let Some(lost) = self.pending.push(track, frame) {
                self.dropped = self.dropped.saturating_add(lost as u64);
                self.tracks.iter_mut().for_each(TrackOut::lost);
            }
            return;
        }
        self.write(rtc, track, &frame);
    }

    fn write(&mut self, rtc: &mut Rtc, track: usize, frame: &Frame) {
        let Some(out) = self.tracks.get_mut(track) else { return };
        let Some((mid, pt)) = out.target else { return };
        let data = out.payload(frame);
        let rtp_time = out.rtp_time(frame.pts);
        let Some(writer) = rtc.writer(mid) else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        match writer.write(pt, frame.received.into_std(), rtp_time, data) {
            Ok(()) => self.sent = self.sent.saturating_add(1),
            Err(error) => {
                if matches!(error, RtcError::WriteWithoutPoll) {
                    self.full = self.full.saturating_add(1);
                } else {
                    debug!(%error, "frame not written");
                }
                self.dropped = self.dropped.saturating_add(1);
                out.lost();
            }
        }
    }

    /// Advances the `Rtc` to now and collects what it has to send. Returns
    /// when it next needs a timeout, or why the session is over.
    fn drive(&mut self, rtc: &mut Rtc, transmits: &mut Vec<Transmit>) -> Result<Instant, String> {
        rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e| e.to_string())?;
        let mut next = self.poll(rtc, transmits)?;
        if self.connected {
            let held: Vec<(usize, Arc<Frame>)> = self.pending.drain().collect();
            for (track, frame) in held {
                // Admitted once when held; asked again because a write
                // that failed during this drain makes video wait for the
                // next keyframe, and the deltas held after it must wait too.
                if !self.tracks.get_mut(track).is_some_and(|t| t.admit(frame.keyframe)) {
                    self.dropped = self.dropped.saturating_add(1);
                    continue;
                }
                self.write(rtc, track, &frame);
                next = self.poll(rtc, transmits)?;
            }
        }
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
                    RtcEvent::KeyframeRequest(request) => {
                        debug!(kind = ?request.kind, "keyframe request ignored: no upstream to ask");
                    }
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
            (sender.drive(rtc, &mut transmits), *last_rx)
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
        let wake_at = tokio::time::Instant::from_std(next.min(idle_at));
        tokio::select! {
            biased;
            () = session.cancel.cancelled() => break "cancelled".into(),
            () = session.wake.notified() => {}
            () = tokio::time::sleep_until(wake_at) => {}
            event = subscription.recv() => match event {
                Err(end) => break end.to_string(),
                Ok(Event::Frame { track, frame }) => {
                    let mut peer = session.peer();
                    sender.frame(&mut peer.rtc, track, frame);
                }
                Ok(Event::Tracks(list)) => {
                    if !sender.set_tracks(&list) {
                        break "the stream's new tracks cannot be sent".into();
                    }
                }
                // The subscription itself resumes video at a keyframe.
                Ok(Event::Discontinuity) => {}
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
    info!(session = session.id, %reason, sent = sender.sent, dropped = sender.dropped, video, "webrtc session ended");
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use oxrtc_core::Codec;
    use str0m::RtcConfig;

    use super::*;

    #[test]
    fn frames_before_connect_are_held_and_bounded() {
        let mut rtc = RtcConfig::new().build(Instant::now());
        let mut sender = Sender::new(Vec::new(), &[TrackInfo::video(Codec::H264)]);
        // No negotiated output: nothing is admitted.
        sender.frame(&mut rtc, 0, Arc::new(Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 1, 0x65]))));
        assert_eq!(sender.pending.len(), 0);
        // With a target, frames wait for the connection.
        sender.tracks[0] = TrackOut::new(
            TrackInfo::video(Codec::H264),
            Some((str0m::media::Mid::from("0"), str0m::media::Pt::from(96))),
        );
        let key = Arc::new(Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 1, 0x65])));
        let delta = Arc::new(Frame::new(0, 1, false, Bytes::from_static(&[0, 0, 0, 1, 0x41])));
        sender.frame(&mut rtc, 0, key);
        for _ in 0..crate::payload::MAX_PENDING_FRAMES {
            sender.frame(&mut rtc, 0, delta.clone());
        }
        // The overflow dropped everything; deltas now wait for a keyframe.
        assert_eq!(sender.pending.len(), 0);
        assert_eq!(sender.dropped, crate::payload::MAX_PENDING_FRAMES as u64 + 1);
        sender.frame(&mut rtc, 0, delta);
        assert_eq!(sender.pending.len(), 0);
    }
}
