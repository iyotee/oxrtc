//! One camera session: DESCRIBE, SETUP (video first), PLAY, then frames into
//! the core until the camera fails, stalls, or the run is cancelled.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Frame, MediaKind, ProducerSink, PublishError, SourceDriver, SourceError,
    TrackInfo,
};
use retina::client::{
    Demuxed, InitialTimestampPolicy, PlayOptions, Session, SessionGroup, SessionIdPolicy, SessionOptions, SetupOptions,
    Stream, TeardownPolicy, Transport,
};
use retina::codec::CodecItem;
use tracing::debug;

use crate::codec::{classify_error, stream_codec, track_of};
use crate::target::CameraTarget;

/// Largest RTSP response (head and body) read from a camera. A DESCRIBE
/// answer is a few KiB; anything this large is not a camera talking.
pub const MAX_RTSP_MESSAGE_BYTES: usize = 256 * 1024;

/// Longest wait, before reconnecting, for the TEARDOWN of the previous
/// session of the same source (cameras that keep sending on a dead session
/// confuse the next one).
pub const MAX_STALE_WAIT: Duration = Duration::from_secs(5);

/// The track whose frames keep a session alive: the first one set up, which
/// is the video track whenever the camera has one.
const PRIMARY_TRACK: u8 = 0;

/// Runs one camera.
pub(crate) struct RtspDriver {
    target: CameraTarget,
    /// The source, redacted: what the logs name.
    label: String,
    user_agent: String,
    /// Tracks this source's torn-down sessions across reconnections.
    group: Arc<SessionGroup>,
}

impl RtspDriver {
    pub(crate) fn new(target: CameraTarget, label: String, user_agent: String) -> Self {
        Self { target, label, user_agent, group: Arc::new(SessionGroup::default()) }
    }

    fn session_options(&self) -> SessionOptions {
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

    async fn connect(&self) -> Result<(Demuxed, Vec<(usize, Codec)>), SourceError> {
        if self.target.tls {
            return Err(SourceError::Unsupported(
                "rtsps (RTSP over TLS): the RTSP client speaks plain rtsp only".into(),
            ));
        }
        let stale = self.group.stale_sessions();
        if tokio::time::timeout(MAX_STALE_WAIT, self.group.await_stale_sessions(&stale)).await.is_err() {
            debug!(source = %self.label, "previous session not torn down yet; connecting anyway");
        }
        let mut session =
            self.step("DESCRIBE", Session::describe(self.target.url.clone(), self.session_options())).await?;
        let chosen = choose_streams(session.streams());
        if chosen.is_empty() {
            return Err(SourceError::Protocol("no video or audio stream in a codec oxrtc carries".into()));
        }
        let mut set_up = Vec::with_capacity(chosen.len());
        for (index, codec) in chosen {
            let options = SetupOptions::default().transport(Transport::default());
            match self.step("SETUP", session.setup(index, options)).await {
                Ok(()) => set_up.push((index, codec)),
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
        let demuxed = playing.demuxed().map_err(|e| classify_error(&e))?;
        Ok((demuxed, set_up))
    }

    async fn session(&self, sink: &ProducerSink) -> Result<(), SourceError> {
        let (mut demuxed, set_up) = self.connect().await?;
        let mut tracks = tracks_of(demuxed.streams(), &set_up);
        sink.set_tracks(tracks.clone()).map_err(refused)?;
        debug!(source = %self.label, ?tracks, "RTSP session playing");
        // The stall clock runs from the last frame of the first set-up track
        // (the video one when there is video), not from the last item: RTCP
        // reports, or a microphone that keeps sending, must not hide a frozen
        // picture. It also bounds how long retina may buffer an access unit
        // that never completes (retina caps neither its size nor its pieces).
        let stall = self.target.stall_timeout;
        let mut deadline = tokio::time::Instant::now() + stall;
        loop {
            let item = tokio::time::timeout_at(deadline, demuxed.next()).await.map_err(|_| SourceError::Stalled)?;
            let item = match item {
                None => return Err(SourceError::Ended),
                Some(Err(e)) => {
                    debug!(source = %self.label, error = %e, "RTSP session failed");
                    return Err(classify_error(&e));
                }
                Some(Ok(item)) => item,
            };
            let frame = match item {
                CodecItem::VideoFrame(video) => {
                    let Some(track) = track_index(&set_up, video.stream_id()) else { continue };
                    if video.has_new_parameters() {
                        let fresh = tracks_of(demuxed.streams(), &set_up);
                        if fresh != tracks {
                            debug!(source = %self.label, ?fresh, "new codec parameters");
                            sink.set_tracks(fresh.clone()).map_err(refused)?;
                            tracks = fresh;
                        }
                    }
                    let pts = video.timestamp().elapsed();
                    let keyframe = video.is_random_access_point();
                    Frame::new(track, pts, keyframe, Bytes::from(video.into_data()))
                }
                CodecItem::AudioFrame(audio) => {
                    let Some(track) = track_index(&set_up, audio.stream_id()) else { continue };
                    Frame::new(track, audio.timestamp().elapsed(), true, Bytes::copy_from_slice(audio.data()))
                }
                _ => continue,
            };
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

/// The core track index of a retina stream index.
fn track_index(set_up: &[(usize, Codec)], stream_id: usize) -> Option<u8> {
    set_up.iter().position(|(i, _)| *i == stream_id).and_then(|p| u8::try_from(p).ok())
}

fn tracks_of(streams: &[Stream], set_up: &[(usize, Codec)]) -> Vec<TrackInfo> {
    set_up.iter().filter_map(|(i, codec)| streams.get(*i).map(|s| track_of(*codec, s))).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_map_to_track_indexes() {
        let set_up = [(2, Codec::H264), (0, Codec::Pcma)];
        assert_eq!(track_index(&set_up, 2), Some(0));
        assert_eq!(track_index(&set_up, 0), Some(1));
        assert_eq!(track_index(&set_up, 1), None);
    }
}
