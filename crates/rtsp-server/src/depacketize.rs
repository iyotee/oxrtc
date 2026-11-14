//! RTP to frames for publications, with retina's depacketizers: the same code
//! that reads the cameras (decision D4: one depacketizer implementation).
//!
//! retina exposes its depacketizers to fuzzers and benchmarks through
//! `codec::Depacketizer` and `testutil::DepacketizeBuf`; those are public but
//! `doc(hidden)`, which is why the dependency is pinned to an exact version.

use std::num::{NonZeroU16, NonZeroU32};

use bytes::Bytes;
use oxrtc_core::frame::{h264_is_keyframe, h265_is_keyframe};
use oxrtc_core::{Codec, CodecParams};
use retina::codec::{CodecItem, Depacketizer, FrameFormat, ParameterSetInsertion, ParametersRef, VideoParametersCodec};
use retina::rtp::{PacketHeader, PacketMeta};
use retina::testutil::DepacketizeBuf;

/// Initial size of the depacketizer's ring buffer; it grows to the largest
/// frame in progress, which [`TrackDepacketizer`] bounds.
const RING_START: usize = 64 * 1024;

/// AVCC with the parameter sets in band on every keyframe: frames that decode
/// on their own, whatever the SDP announced.
const FORMAT: FrameFormat =
    FrameFormat { parameter_set_insertion: ParameterSetInsertion::EachKeyFrame, ..FrameFormat::MP4 };

/// What builds a depacketizer, kept to rebuild it after an error.
#[derive(Debug, Clone)]
pub(crate) struct DepacketizerSpec {
    /// `video` or `audio`.
    pub(crate) media: String,
    /// Lower-case encoding name of the `rtpmap` (`h264`, `mpeg4-generic`...).
    pub(crate) encoding: String,
    /// The RTP clock rate.
    pub(crate) clock_rate: NonZeroU32,
    /// Audio channels, if announced.
    pub(crate) channels: Option<NonZeroU16>,
    /// The `fmtp` parameters, if any.
    pub(crate) fmtp: Option<String>,
}

/// A frame out of the depacketizer.
#[derive(Debug, Clone)]
pub(crate) struct Depacketized {
    /// Timestamp since the first packet, in clock-rate units.
    pub(crate) pts: i64,
    /// Whether decoding can start here.
    pub(crate) keyframe: bool,
    /// AVCC for H.264/H.265, the raw frame otherwise.
    pub(crate) data: Bytes,
}

/// One announced track's depacketizer.
pub(crate) struct TrackDepacketizer {
    spec: DepacketizerSpec,
    codec: Codec,
    inner: Depacketizer,
    buf: DepacketizeBuf,
    start: Option<u32>,
    last_ts: u32,
    extended: i64,
    last_seq: Option<u16>,
    pending: usize,
    max_frame: usize,
}

impl std::fmt::Debug for TrackDepacketizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackDepacketizer").field("codec", &self.codec).field("pending", &self.pending).finish()
    }
}

fn build(spec: &DepacketizerSpec) -> Result<Depacketizer, String> {
    let mut inner =
        Depacketizer::new(&spec.media, &spec.encoding, spec.clock_rate.get(), spec.channels, spec.fmtp.as_deref())?;
    inner.set_frame_format(FORMAT);
    Ok(inner)
}

impl TrackDepacketizer {
    /// A depacketizer for `codec`, refusing frames over `max_frame` bytes.
    pub(crate) fn new(spec: DepacketizerSpec, codec: Codec, max_frame: usize) -> Result<Self, String> {
        let inner = build(&spec)?;
        Ok(Self {
            spec,
            codec,
            inner,
            buf: DepacketizeBuf::new(RING_START),
            start: None,
            last_ts: 0,
            extended: 0,
            last_seq: None,
            pending: 0,
            max_frame,
        })
    }

    /// The codec configuration known so far (from the SDP, or the stream).
    pub(crate) fn params(&self) -> CodecParams {
        match self.inner.parameters() {
            Some(ParametersRef::Video(video)) => match video.codec_params() {
                VideoParametersCodec::H264 { sps, pps } => CodecParams::H264 { sps: sps.clone(), pps: pps.clone() },
                VideoParametersCodec::H265 { vps, sps, pps } => {
                    CodecParams::H265 { vps: vps.clone(), sps: sps.clone(), pps: pps.clone() }
                }
                _ => CodecParams::None,
            },
            Some(ParametersRef::Audio(audio)) if self.codec == Codec::Aac && !audio.extra_data().is_empty() => {
                CodecParams::Aac { config: Bytes::copy_from_slice(audio.extra_data()) }
            }
            _ => CodecParams::None,
        }
    }

    /// Starts over after an error: the partial frame is lost.
    fn reset(&mut self) {
        if let Ok(inner) = build(&self.spec) {
            self.inner = inner;
        }
        self.buf = DepacketizeBuf::new(RING_START);
        self.pending = 0;
    }

    /// Feeds one RTP packet; completed frames are appended to `out`.
    pub(crate) fn push(&mut self, packet: &[u8], out: &mut Vec<Depacketized>) -> Result<(), String> {
        let (header, range) = PacketHeader::validate(packet).map_err(str::to_owned)?;
        let payload = packet
            .get(usize::from(range.start)..usize::from(range.end))
            .ok_or_else(|| "bad payload range".to_owned())?;
        let ts = header.timestamp();
        let start = match self.start {
            Some(start) => {
                // Signed difference: a timestamp may step back a little (B-frames).
                self.extended = self.extended.saturating_add(i64::from(ts.wrapping_sub(self.last_ts) as i32));
                start
            }
            None => {
                self.start = Some(ts);
                self.extended = i64::from(ts);
                ts
            }
        };
        self.last_ts = ts;
        let seq = header.sequence_number();
        let loss = match self.last_seq.replace(seq) {
            Some(previous) => match seq.wrapping_sub(previous).wrapping_sub(1) {
                gap if gap < 0x8000 => gap,
                _ => 0,
            },
            None => 0,
        };
        let timestamp =
            retina::Timestamp::new(self.extended, self.spec.clock_rate, start).ok_or_else(|| "timestamp".to_owned())?;
        self.pending = self.pending.saturating_add(payload.len());
        if self.pending > self.max_frame {
            self.reset();
            return Err(format!("frame over {} bytes", self.max_frame));
        }
        let meta = PacketMeta {
            ctx: retina::PacketContext::dummy(),
            stream_id: 0,
            timestamp,
            sequence_number: seq,
            ssrc: header.ssrc(),
            mark: header.mark(),
            loss,
        };
        if let Err(error) = self.buf.push(&mut self.inner, meta, payload) {
            self.reset();
            return Err(error);
        }
        let mut failure = None;
        while let Some(item) = self.inner.pull() {
            match item {
                Ok(CodecItem::VideoFrame(frame)) => {
                    self.pending = 0;
                    let pts = frame.timestamp().elapsed();
                    let rap = frame.is_random_access_point();
                    let data = Bytes::from(frame.into_data());
                    let keyframe = rap
                        || match self.codec {
                            Codec::H264 => h264_is_keyframe(&data),
                            Codec::H265 => h265_is_keyframe(&data),
                            _ => false,
                        };
                    out.push(Depacketized { pts, keyframe, data });
                }
                Ok(CodecItem::AudioFrame(frame)) => {
                    self.pending = 0;
                    let pts = frame.timestamp().elapsed();
                    out.push(Depacketized { pts, keyframe: true, data: Bytes::copy_from_slice(frame.data()) });
                }
                Ok(_) => {}
                Err(error) => failure = Some(format!("{error:?}")),
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn spec(media: &str, encoding: &str, clock: u32, fmtp: Option<&str>) -> DepacketizerSpec {
        DepacketizerSpec {
            media: media.into(),
            encoding: encoding.into(),
            clock_rate: NonZeroU32::new(clock).unwrap(),
            channels: None,
            fmtp: fmtp.map(str::to_owned),
        }
    }

    fn rtp(seq: u16, ts: u32, marker: bool, pt: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, (u8::from(marker) << 7) | pt];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&[1, 2, 3, 4]);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn pcma_frames_come_out_one_per_packet() {
        let mut d = TrackDepacketizer::new(spec("audio", "pcma", 8000, None), Codec::Pcma, 1 << 20).unwrap();
        let mut out = Vec::new();
        d.push(&rtp(1, 1000, false, 8, &[0xd5; 160]), &mut out).unwrap();
        d.push(&rtp(2, 1160, false, 8, &[0xd5; 160]), &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].pts, out[1].pts), (0, 160));
        assert_eq!(out[1].data.len(), 160);
    }

    #[test]
    fn oversized_frame_in_progress_is_dropped() {
        let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 4000).unwrap();
        let mut out = Vec::new();
        // FU-A start then middles, never an end, at one timestamp.
        let mut result = Ok(());
        for seq in 0..10u16 {
            let fu = if seq == 0 { 0x85 } else { 0x05 };
            let mut payload = vec![0x7c, fu];
            payload.extend(std::iter::repeat_n(0x11, 1000));
            result = result.and(d.push(&rtp(seq, 0, false, 96, &payload), &mut out));
        }
        assert!(result.is_err());
        assert!(out.is_empty());
        assert!(d.pending <= 4000);
    }

    #[test]
    fn malformed_packets_are_errors() {
        let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 1 << 20).unwrap();
        let mut out = Vec::new();
        assert!(d.push(&[1, 2, 3], &mut out).is_err());
        assert!(d.push(&[0x40; 20], &mut out).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        /// Arbitrary payloads behind valid RTP headers never panic any of the
        /// depacketizers a publication can use.
        #[test]
        fn random_payloads_never_panic(
            packets in proptest::collection::vec(
                (any::<u16>(), any::<u32>(), any::<bool>(), proptest::collection::vec(any::<u8>(), 0..1500)),
                1..40,
            ),
            which in 0usize..5,
        ) {
            let (media, encoding, clock, codec) = [
                ("video", "h264", 90_000, Codec::H264),
                ("video", "h265", 90_000, Codec::H265),
                ("audio", "opus", 48_000, Codec::Opus),
                ("audio", "pcmu", 8000, Codec::Pcmu),
                ("audio", "mpeg4-generic", 16_000, Codec::Aac),
            ][which];
            let fmtp = (codec == Codec::Aac)
                .then_some("streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1408");
            let mut d = TrackDepacketizer::new(spec(media, encoding, clock, fmtp), codec, 1 << 16).unwrap();
            let mut out = Vec::new();
            for (seq, ts, marker, payload) in packets {
                let _ = d.push(&rtp(seq, ts, marker, 96, &payload), &mut out);
            }
        }
    }
}
