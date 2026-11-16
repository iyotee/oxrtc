//! One camera session: DESCRIBE, SETUP (video first), PLAY, then frames into
//! the core until the camera fails, stalls, or the run is cancelled.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Frame, MediaKind, ProducerSink, PublishError, Refusals, SourceDriver,
    SourceError, TrackInfo,
};
use oxrtc_depacketize::TrackDepacketizer;
use oxrtc_net::Tls;
use retina::client::{
    InitialTimestampPolicy, PacketItem, PlayOptions, Playing, Session, SessionGroup, SessionIdPolicy, SessionOptions,
    SetupOptions, Stream, TeardownPolicy, Transport,
};
use tracing::{debug, warn};

use crate::codec::{classify_error, depacketizer_of, stream_codec, track_of};
use crate::stale::{MAX_STALE_WAIT, StaleWait};
use crate::target::CameraTarget;
use crate::tls::TlsConnector;

/// Largest RTSP response (head and body) read from a camera. A DESCRIBE
/// answer is a few KiB; anything this large is not a camera talking.
pub const MAX_RTSP_MESSAGE_BYTES: usize = 256 * 1024;

/// The track whose frames keep a session alive: the first one set up, which
/// is the video track whenever the camera has one.
const PRIMARY_TRACK: u8 = 0;

/// One set-up stream of the camera, and what turns its RTP packets into frames.
struct Lane {
    /// The stream's index in retina's session.
    stream: usize,
    codec: Codec,
    depacketizer: TrackDepacketizer,
}

/// Runs one camera.
pub(crate) struct RtspDriver {
    target: CameraTarget,
    /// The source, redacted: what the logs name.
    label: String,
    user_agent: String,
    /// Tracks this source's torn-down sessions across reconnections.
    group: Arc<SessionGroup>,
    /// How an `rtsps` source reaches its camera; none for plain `rtsp`.
    connector: Option<Arc<TlsConnector>>,
    /// Whether the unchecked certificate has been said already.
    said_unverified: AtomicBool,
    /// What this camera has shown of its TEARDOWN: how long to wait for it.
    stale_wait: StaleWait,
}

impl RtspDriver {
    pub(crate) fn new(target: CameraTarget, label: String, user_agent: String, tls: Arc<Tls>) -> Self {
        let connector = target.tls.then(|| Arc::new(TlsConnector::new(tls, target.verify, target.stall_timeout)));
        Self {
            target,
            label,
            user_agent,
            group: Arc::new(SessionGroup::default()),
            connector,
            said_unverified: AtomicBool::new(false),
            stale_wait: StaleWait::default(),
        }
    }

    fn session_options(&self) -> SessionOptions {
        let options = self.base_options();
        match &self.connector {
            Some(connector) => options.connector(connector.clone()),
            None => options,
        }
    }

    fn base_options(&self) -> SessionOptions {
        SessionOptions::default()
            .creds(self.target.creds.clone())
            .user_agent(self.user_agent.clone())
            .session_group(self.group.clone())
            .teardown(TeardownPolicy::Auto)
            // Some cameras answer each SETUP with a different session id.
            .session_id(SessionIdPolicy::UseFirst)
            .max_message_size(MAX_RTSP_MESSAGE_BYTES)
    }

    /// Bounds one handshake step by the stall timeout and classifies its error.
    async fn step<T>(&self, what: &str, fut: impl Future<Output = Result<T, retina::Error>>) -> Result<T, SourceError> {
        let timeout = self.target.stall_timeout;
        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                debug!(source = %self.label, error = %e, "RTSP {what} failed");
                Err(classify_error(&e))
            }
            Err(_) => Err(SourceError::Unreachable(format!("no answer to {what} within {}s", timeout.as_secs()))),
        }
    }

    async fn connect(&self, max_frame: usize) -> Result<(Session<Playing>, Vec<Lane>), SourceError> {
        if self.connector.is_some() && !self.target.verify && !self.said_unverified.swap(true, Ordering::Relaxed) {
            // Once per source, not per reconnection: a camera with a
            // certificate of its own would fill the log.
            warn!(source = %self.label, "rtsps: the certificate is not checked (add #verify to check it)");
        }
        let stale = self.group.stale_sessions();
        let mut shortened = false;
        if stale.num_sessions > 0 {
            let budget = self.stale_wait.budget();
            shortened = budget < MAX_STALE_WAIT;
            let settled = tokio::time::timeout(budget, self.group.await_stale_sessions(&stale)).await.is_ok();
            self.stale_wait.waited(budget, settled);
            if !settled {
                debug!(source = %self.label, "previous session not torn down yet; connecting anyway");
            }
        }
        let result = self.handshake(max_frame).await;
        if result.is_err() && shortened {
            // The camera may have refused for want of the old session's end.
            self.stale_wait.connection_failed();
        }
        result
    }

    /// DESCRIBE, SETUP and PLAY, once the previous session has been waited for.
    async fn handshake(&self, max_frame: usize) -> Result<(Session<Playing>, Vec<Lane>), SourceError> {
        let mut session =
            self.step("DESCRIBE", Session::describe(self.target.url.clone(), self.session_options())).await?;
        let chosen = choose_streams(session.streams());
        if chosen.is_empty() {
            return Err(SourceError::Protocol("no video or audio stream in a codec oxrtc carries".into()));
        }
        let mut set_up: Vec<Lane> = Vec::with_capacity(chosen.len());
        for (index, codec) in chosen {
            // Before the camera is asked for anything: a stream no depacketizer
            // takes is refused as retina's own did, at `demuxed()`.
            let depacketizer = depacketizer_of(session.streams().get(index), codec, max_frame)?;
            let options = SetupOptions::default().transport(Transport::default());
            match self.step("SETUP", session.setup(index, options)).await {
                Ok(()) => set_up.push(Lane { stream: index, codec, depacketizer }),
                // A camera that refuses its microphone still has pictures. Only
                // a refusal the camera answered: after a timeout or a broken
                // connection the next answer may be the late SETUP's.
                Err(e)
                    if codec.kind() == MediaKind::Audio
                        && !set_up.is_empty()
                        && !matches!(e, SourceError::Unreachable(_)) =>
                {
                    debug!(source = %self.label, error = %e, "audio SETUP refused; continuing without audio");
                }
                Err(e) => return Err(e),
            }
        }
        // Many cameras omit or botch RTP-Info's rtptime with several streams.
        let play = PlayOptions::default().initial_timestamp(InitialTimestampPolicy::Permissive);
        let playing = self.step("PLAY", session.play(play)).await?;
        Ok((playing, set_up))
    }

    async fn session(&self, sink: &ProducerSink) -> Result<(), SourceError> {
        let (mut playing, mut lanes) = self.connect(sink.max_frame_bytes()).await?;
        let mut tracks = tracks_of(playing.streams(), &lanes);
        sink.set_tracks(tracks.clone()).map_err(refused)?;
        debug!(source = %self.label, ?tracks, "RTSP session playing");
        // The stall clock runs from the last frame of the first set-up track
        // (the video one when there is video), not from the last item: RTCP
        // reports, or a microphone that keeps sending, must not hide a frozen
        // picture.
        let stall = self.target.stall_timeout;
        let mut deadline = tokio::time::Instant::now() + stall;
        // The access unit in progress is capped by the depacketizer
        // (`max_frame`, R11), so a camera that never ends one costs that much
        // memory, not the stall timeout's worth of it.
        let mut frames = Vec::new();
        let cut = Refusals::new();
        loop {
            let item = tokio::time::timeout_at(deadline, playing.next()).await.map_err(|_| SourceError::Stalled)?;
            let packet = match item {
                None => return Err(SourceError::Ended),
                Some(Err(e)) => {
                    debug!(source = %self.label, error = %e, "RTSP session failed");
                    return Err(classify_error(&e));
                }
                Some(Ok(PacketItem::Rtp(packet))) => packet,
                // Sender reports are retina's to read, for its own timestamps.
                Some(Ok(_)) => continue,
            };
            let Some(index) = lanes.iter().position(|lane| lane.stream == packet.stream_id()) else { continue };
            let Some(lane) = lanes.get_mut(index) else { continue };
            frames.clear();
            // A packet that does not depacketize (or an access unit over the
            // cap) loses its picture, not the session: the depacketizer starts
            // over and the next access unit comes out whole.
            if let Err(error) = lane.depacketizer.push(packet.raw(), &mut frames) {
                // Counted in the source's `rejected`, as a frame the hub
                // refuses is; said at most once a second.
                sink.reject();
                if let Some(count) = cut.refused() {
                    debug!(source = %self.label, %error, count, "RTP packets refused");
                }
            }
            let Ok(track) = u8::try_from(index) else { continue };
            for depacketized in frames.drain(..) {
                if depacketized.new_parameters {
                    let fresh = tracks_of(playing.streams(), &lanes);
                    if fresh != tracks {
                        debug!(source = %self.label, ?fresh, "new codec parameters");
                        sink.set_tracks(fresh.clone()).map_err(refused)?;
                        tracks = fresh;
                    }
                }
                let frame = Frame::new(track, depacketized.pts, depacketized.keyframe, depacketized.data);
                if frame.track == PRIMARY_TRACK {
                    deadline = tokio::time::Instant::now() + stall;
                }
                match sink.publish(frame) {
                    Ok(()) => {}
                    // One bad frame is counted by the core and skipped; the camera stays up.
                    Err(e @ (PublishError::TooLarge { .. } | PublishError::Malformed(_))) => {
                        debug!(source = %self.label, error = %e, "frame refused");
                    }
                    Err(e) => return Err(refused(e)),
                }
            }
        }
    }
}

fn refused(e: PublishError) -> SourceError {
    SourceError::Refused(e.to_string())
}

/// The streams to set up, in SETUP order: the first video stream oxrtc
/// can carry, then the first such audio stream. Video goes first because
/// reference cameras mis-route interleaved channels when audio is set up first.
pub(crate) fn choose_streams(streams: &[Stream]) -> Vec<(usize, Codec)> {
    let first = |kind: MediaKind| {
        streams.iter().enumerate().find_map(|(i, s)| stream_codec(s).filter(|c| c.kind() == kind).map(|c| (i, c)))
    };
    MediaKind::ALL.into_iter().filter_map(first).collect()
}

/// The core's track list: the set-up streams in SETUP order, with the codec
/// parameters their depacketizers know so far (the SDP's, then the stream's).
fn tracks_of(streams: &[Stream], lanes: &[Lane]) -> Vec<TrackInfo> {
    lanes
        .iter()
        .filter_map(|lane| streams.get(lane.stream).map(|s| track_of(lane.codec, s, lane.depacketizer.params())))
        .collect()
}

impl SourceDriver for RtspDriver {
    fn stall_timeout(&self) -> Option<std::time::Duration> {
        Some(self.target.stall_timeout)
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        // Dropping the session (on cancel, on error, or when this future is
        // dropped) makes retina send TEARDOWN in the background.
        Box::pin(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Ok(()),
                result = self.session(&sink) => result,
            }
        })
    }
}
