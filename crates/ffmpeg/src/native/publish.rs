//! What a native run publishes: the output's track, then a frame per packet,
//! each dated on the input's clock and received when its first sample was.

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

/// One input's transcode into a sink.
pub(crate) struct Publisher {
    transcoder: Transcoder,
    rate: u32,
    packets: Vec<Packet>,
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
        Ok(Self { transcoder, rate: input.rate, packets: Vec::new() })
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
    /// publishes the packets it completes.
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
            let frame =
                Frame { track: 0, pts: packet.pts, keyframe: true, received: at, data: Bytes::from(packet.data) };
            sink.publish(frame).map_err(|e| SourceError::Refused(e.to_string()))?;
        }
        Ok(())
    }
}
