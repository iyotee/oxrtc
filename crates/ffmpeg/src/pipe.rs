//! A run whose pictures come on FFmpeg's standard output (`#video=mjpeg`
//! alone, go2rtc's `-f mjpeg -`, D46): the byte stream cut into its JPEG
//! pictures ([`oxrtc_core::jpeg::Splitter`]) and published as they come, one
//! keyframe each at 90 kHz on the wall clock, as a webcam's are.

use std::process::Stdio;

use bytes::BytesMut;
use oxrtc_core::jpeg::Splitter;
use oxrtc_core::timeline::ticks;
use oxrtc_core::{CancellationToken, Codec, Frame, ProducerSink, PublishError, SourceError, TrackInfo};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;

use super::{FfmpegDriver, LAST_WORDS, exited, stop};
use crate::process::{Tail, command, drain, spawn};
use crate::version::reads_framerate;

/// Largest picture read from FFmpeg (the core's own bound for a frame).
pub(crate) const MAX_PICTURE_BYTES: usize = 8 * 1024 * 1024;

/// How much is read from the pipe at a time.
const READ_BYTES: usize = 256 * 1024;

impl FfmpegDriver {
    pub(super) async fn run_pipe(&self, sink: ProducerSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let _permit = self.limiter.try_acquire()?;
        // Nothing is published on the lease's URL: it is held for the
        // credential this run reads the server's streams with (D24), which
        // ends with it.
        let lease = self.target.lease(sink.clone());
        let credential = lease.read_credential();
        let skip_rate_guess = self.plan.local() && reads_framerate(self.version.major(&self.launch).await);
        let args =
            self.plan.args_reading_as("", credential.as_ref().map(|(u, p)| (u.as_str(), p.as_str())), skip_rate_guess);
        tracing::debug!(source = %self.label, "ffmpeg: starting, pictures on its output");
        let mut process = spawn(command(&self.launch, &args, Stdio::null(), Stdio::piped()), &self.launch.bin)
            .await
            .map_err(|e| SourceError::Unreachable(format!("ffmpeg: {e}")))?;
        let tail = Tail::default();
        let (Some(mut stdout), stderr) = (process.child.stdout.take(), process.child.stderr.take()) else {
            stop(&mut process).await;
            return Err(SourceError::Unreachable("ffmpeg: no output pipe".into()));
        };
        let label = self.label.clone();
        let drain_tail = tail.clone();
        let drained = async move {
            if let Some(stderr) = stderr {
                drain(stderr, drain_tail, &label).await;
            }
        };
        tokio::pin!(drained);
        let mut drain_done = false;

        let started = Instant::now();
        let deadline = started + self.start_timeout;
        let mut splitter = Splitter::new(MAX_PICTURE_BYTES);
        let mut chunk = BytesMut::with_capacity(READ_BYTES);
        let mut publishing = false;
        loop {
            chunk.clear();
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    stop(&mut process).await;
                    return Ok(());
                }
                () = &mut drained, if !drain_done => drain_done = true,
                () = tokio::time::sleep_until(deadline), if !publishing => {
                    stop(&mut process).await;
                    let why = tail.joined().map(|l| format!(" ({l})")).unwrap_or_default();
                    return Err(SourceError::Unreachable(format!(
                        "ffmpeg: no picture within {:?}{why}",
                        self.start_timeout
                    )));
                }
                read = stdout.read_buf(&mut chunk) => {
                    if matches!(read, Ok(0) | Err(_)) {
                        // FFmpeg closed its output: it is exiting, and says why.
                        let status = tokio::time::timeout(super::REAP_GRACE, process.child.wait()).await;
                        if !drain_done {
                            let _ = tokio::time::timeout(LAST_WORDS, &mut drained).await;
                        }
                        let status = status.unwrap_or_else(|_| Err(std::io::Error::other("still running")));
                        stop(&mut process).await;
                        return Err(exited(status, &tail));
                    }
                    splitter.push(&chunk);
                    loop {
                        let picture = match splitter.next_picture() {
                            Ok(Some(picture)) => picture,
                            Ok(None) => break,
                            Err(error) => {
                                tracing::debug!(source = %self.label, %error, "ffmpeg: picture dropped");
                                continue;
                            }
                        };
                        if !publishing {
                            sink.set_tracks(vec![TrackInfo::video(Codec::Jpeg)])
                                .map_err(|e| SourceError::Refused(e.to_string()))?;
                            publishing = true;
                            tracing::debug!(source = %self.label, "ffmpeg: publishing");
                        }
                        match sink.publish(Frame::new(0, ticks(started.elapsed(), 90_000), true, picture)) {
                            Ok(()) => {}
                            Err(e @ (PublishError::TooLarge { .. } | PublishError::Malformed(_))) => {
                                tracing::debug!(source = %self.label, error = %e, "ffmpeg: picture refused");
                            }
                            Err(e) => {
                                stop(&mut process).await;
                                return Err(SourceError::Refused(e.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }
}
