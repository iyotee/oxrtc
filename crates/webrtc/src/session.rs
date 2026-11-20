//! One WebRTC session: drives its `Rtc` and forwards the subscription's
//! frames to it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oxrtc_core::{Event, Frame, MediaKind, Subscription, TrackInfo};
use str0m::net::Transmit;
use str0m::{Event as RtcEvent, IceConnectionState, Input, Output as RtcOutput, Rtc, RtcError};
use tracing::{debug, info, warn};

use self::sound::dated;
use crate::Playout;
use crate::pace::{Held, SoundClock};
use crate::payload::{Output, TrackOut, choose};
use crate::registry::Session;
use crate::replay::Replays;
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
///
/// The picture that waited is sent: nothing decodes without its keyframe.
/// The sound that waited is not ([`Sender::stale`]): sent in one burst at the
/// connection, it starts the browser's audio buffer as deep as the connection
/// was long, and the buffer comes back down only to the top of the band it
/// tolerates, where it stays (about 90 ms of sound held against 53 ms for a
/// stream that starts empty, measured in Edge). Since the picture and the
/// sound are one `MediaStream` (`msid.rs`) the browser holds the picture for
/// the sound, so that depth is the picture's latency too. For the same
/// reason the sound then leaves on its stream's own clock ([`crate::pace`]).
struct Sender {
    outputs: Vec<Output>,
    tracks: Vec<TrackOut>,
    playout: Playout,
    /// Per track: the clock of a sound track this session sends.
    clocks: Vec<Option<SoundClock>>,
    /// The sound waiting for its time.
    held: Held,
    /// How much earlier than it arrived a picture is dated ([`dated`]).
    picture_lead: Duration,
    /// When ICE and DTLS were up; `None` until then.
    connected: Option<tokio::time::Instant>,
    replays: Replays,
    sent: u64,
    dropped: u64,
    full: u64,
    /// Queued frames a replay had sent already.
    repeated: u64,
    /// Sound that entered before the connection, not sent.
    stale_sound: u64,
    /// Sound too far behind its stream's clock, not sent.
    late_sound: u64,
    /// The longest the sound was held to leave evenly.
    sound_hold: Duration,
    /// The holds of every sound frame placed, added up, and their count: the
    /// mean is what the sound's path costs on average (in lip sync, the
    /// picture's latency too).
    sound_hold_sum: Duration,
    sound_placed: u64,
    /// The latest a held sound frame left after its time (the timer's own
    /// lateness).
    pace_overrun: Duration,
}

impl Sender {
    fn new(outputs: Vec<Output>, tracks: &[TrackInfo], playout: Playout) -> Self {
        let mut sender = Self {
            outputs,
            tracks: Vec::new(),
            playout,
            clocks: Vec::new(),
            held: Held::default(),
            picture_lead: Duration::ZERO,
            connected: None,
            replays: Replays::default(),
            sent: 0,
            dropped: 0,
            full: 0,
            repeated: 0,
            stale_sound: 0,
            late_sound: 0,
            sound_hold: Duration::ZERO,
            sound_hold_sum: Duration::ZERO,
            sound_placed: 0,
            pace_overrun: Duration::ZERO,
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
        self.set_clocks();
        self.tracks.iter().any(|t| t.target.is_some())
    }

    /// A frame from the subscription.
    fn frame(&mut self, rtc: &mut Rtc, track: usize, frame: &Arc<Frame>) {
        if self.stale(track, frame) {
            self.stale_sound = self.stale_sound.saturating_add(1);
            return;
        }
        let Some(out) = self.tracks.get_mut(track) else { return };
        if out.info.kind == MediaKind::Video {
            out.keys.saw(frame.pts, frame.keyframe);
        }
        if out.repeats(frame.pts) {
            self.repeated = self.repeated.saturating_add(1);
            return;
        }
        if !out.admit(frame.keyframe) {
            return;
        }
        // Sound leaves on its stream's own clock: `release` writes it.
        if self.hold(track, frame) {
            return;
        }
        let Some(out) = self.tracks.get_mut(track) else { return };
        let time = out.time_of(frame.pts);
        self.write(rtc, track, frame, time, frame.received.into_std());
    }

    /// Writes `frame` at RTP `time`, arrived at `wall` and dated from it
    /// ([`dated`]). Whether it was taken.
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
        let dated = dated(out.info.kind, wall, self.picture_lead);
        let data = out.payload(frame);
        let Some(writer) = rtc.writer(mid) else {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        };
        // Lip sync off: a playout delay of 0 to 0 asks the browser to show
        // the picture as soon as it is decoded, whatever the sound's buffer.
        let writer = if !self.playout.lip_sync && out.info.kind == MediaKind::Video {
            writer.playout_delay(str0m::media::MediaTime::ZERO, str0m::media::MediaTime::ZERO)
        } else {
            writer
        };
        match writer.write(pt, dated, time, data) {
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
                        self.connected.get_or_insert_with(tokio::time::Instant::now);
                    }
                    RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Err("ice disconnected".into());
                    }
                    RtcEvent::Closed => return Err("closed by the peer".into()),
                    RtcEvent::KeyframeRequest(request) => self.keyframe_request(&request, Instant::now()),
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
    let mut sender = Sender::new(outputs, &tracks, shared.config.playout);
    let mut transmits = Vec::new();
    let idle = shared.config.session_idle_timeout;
    // str0m's next timeout, and whether the loop woke only for sound due
    // before it: str0m then has nothing to do and is not driven, a round
    // saved on each of the 50 wakes a second paced sound adds (W1b: B01's
    // server CPU 1.2 and 2.1 % of a core against 1.7 and 2.4 driven, 0.7 and
    // 1.1 unpaced, on a machine 70 to 100 % busy).
    let (mut rtc_next, mut for_sound): (Option<Instant>, bool) = (None, false);
    let reason: String = loop {
        let (step, last_rx) = {
            let mut peer = session.peer();
            let Peer { rtc, last_rx } = &mut *peer;
            let mut step = match rtc_next {
                Some(at) if for_sound && Instant::now() < at => Ok(at),
                _ => sender.drive(rtc, &mut transmits),
            };
            for_sound = false;
            if let Ok(next) = step {
                step = sender.release(rtc, &mut transmits).map(|last| last.map_or(next, |at| at.min(next)));
            }
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
        rtc_next = Some(next);
        let sound_first = sender.sound_due().is_some_and(|sound_at| sound_at < wake_at);
        if let Some(sound_at) = sender.sound_due() {
            wake_at = wake_at.min(sound_at);
        }
        let wake_at = tokio::time::Instant::from_std(wake_at);
        tokio::select! {
            biased;
            () = session.cancel.cancelled() => break "cancelled".into(),
            () = session.wake.notified() => {}
            () = tokio::time::sleep_until(wake_at) => for_sound = sound_first,
            event = subscription.recv(), if sender.connected.is_some() => match event {
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
    // The consumer has left the stream: a `DELETE` waiting for that is done,
    // and the session no longer counts against the limits.
    shared.retire(session.id);
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
        keyframe_waits = sender.replays.declined,
        replayed_frames = sender.replays.frames,
        repeated = sender.repeated,
        stale_sound = sender.stale_sound,
        late_sound = sender.late_sound,
        sound_hold_ms = sender.sound_hold.as_millis(),
        sound_hold_mean_ms = sender.sound_hold_mean().as_millis(),
        pace_overrun_ms = sender.pace_overrun.as_millis(),
        "webrtc session ended"
    );
}

#[path = "session_sound.rs"]
mod sound;

#[path = "session_keyframes.rs"]
mod keyframes;

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
