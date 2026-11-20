//! What a native run publishes: the output's track, then a frame per packet,
//! each dated on the input's clock and received when its first sample was -
//! and published no sooner than that instant. A camera's 64 ms frame makes
//! three or four packets at once, the later ones received up to 44 ms after
//! the frame; published at once, they reached the WebRTC sender before their
//! instant, and str0m's sender report written after one says RTP time 0
//! (its wall clock in the future): the browser then placed the sound seconds
//! from its picture (W5). The browser plays a packet by its date, so one that
//! waits for its instant is not heard later.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use oxrtc_audio::transcode::{Input, Output, OutputCodec, Packet, TranscodeError, Transcoder};
use oxrtc_core::{Codec, Frame, ProducerSink, SourceError, TrackInfo};
use tokio::time::Instant;

/// The output's track, as FFmpeg's RTSP muxer describes it.
pub(crate) fn track_of(output: Output) -> TrackInfo {
    let codec = match output.codec {
        OutputCodec::Opus => Codec::Opus,
        OutputCodec::Alaw => Codec::Pcma,
        OutputCodec::Ulaw => Codec::Pcmu,
        OutputCodec::L16 | OutputCodec::G722 => Codec::L16,
    };
    TrackInfo::audio(codec, output.clock_rate(), output.described_channels())
}

fn failed(error: TranscodeError) -> SourceError {
    SourceError::Unsupported(format!("native audio: {error}"))
}

/// Sleeps until `at`, or for ever when nothing waits.
pub(crate) async fn wait_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// One input's transcode into a sink.
pub(crate) struct Publisher {
    transcoder: Transcoder,
    rate: u32,
    packets: Vec<Packet>,
    /// Packets made, in order, waiting for the instant they were received.
    waiting: VecDeque<Frame>,
}

impl Publisher {
    /// A publisher of `input` as `output` into `sink`, its track set.
    ///
    /// # Errors
    ///
    /// The transcode cannot be made, or the sink refused the track.
    pub(crate) fn new(input: Input, output: Output, sink: &ProducerSink) -> Result<Self, String> {
        let transcoder = Transcoder::new(input, output).map_err(|e| e.to_string())?;
        sink.set_tracks(vec![track_of(output)]).map_err(|e| e.to_string())?;
        Ok(Self { transcoder, rate: input.rate, packets: Vec::new(), waiting: VecDeque::new() })
    }

    /// Whether `input` is the one this publisher transcodes.
    pub(crate) fn reads(&self, input: Input) -> bool {
        self.transcoder.input() == input
    }

    /// The input was cut (frames lost): the next frame starts a new run.
    pub(crate) fn discontinuity(&mut self) {
        self.transcoder.reset();
    }

    /// Transcodes one input frame dated `pts`, received at `received`, and
    /// publishes the packets it completes whose instant has come; the others
    /// wait, in order, for [`Self::publish_due`] at [`Self::next_due`].
    pub(crate) fn push(
        &mut self,
        payload: &[u8],
        pts: i64,
        received: Instant,
        sink: &ProducerSink,
    ) -> Result<(), SourceError> {
        self.packets.clear();
        self.transcoder.push(payload, pts, &mut self.packets).map_err(failed)?;
        for packet in self.packets.drain(..) {
            // When its first sample was received: the frame's own arrival,
            // moved by where that sample sits from the frame's start.
            let offset = packet.source_pts - pts;
            let shift = Duration::from_micros(offset.unsigned_abs().saturating_mul(1_000_000) / u64::from(self.rate));
            let at = if offset >= 0 { received + shift } else { received.checked_sub(shift).unwrap_or(received) };
            self.waiting.push_back(Frame {
                track: 0,
                pts: packet.pts,
                keyframe: true,
                received: at,
                data: Bytes::from(packet.data),
            });
        }
        self.publish_due(sink)
    }

    /// Publishes, in order, the waiting packets whose instant has come.
    pub(crate) fn publish_due(&mut self, sink: &ProducerSink) -> Result<(), SourceError> {
        let now = Instant::now();
        while let Some(frame) = self.waiting.pop_front() {
            if frame.received > now {
                self.waiting.push_front(frame);
                break;
            }
            sink.publish(frame).map_err(|e| SourceError::Refused(e.to_string()))?;
        }
        Ok(())
    }

    /// The instant the next waiting packet was received, if one waits.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.waiting.front().map(|frame| frame.received)
    }
}
