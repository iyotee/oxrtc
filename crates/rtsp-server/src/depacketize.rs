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

/// Most packets reported lost to retina between two frames. retina sums the
/// losses it is told of during a run of lost packets in a `u16` (0.4.22:
/// `h264.rs`/`h265.rs` `pkts += meta.loss`, `meta.loss + additional_loss`),
/// unchecked; its sum never exceeds ours, so keeping ours under this bound
/// keeps it from overflowing whatever gaps a peer's sequence numbers make.
const MAX_LOSS_RUN: u16 = 0x7fff;

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
    /// Losses reported to retina since its last frame (at most
    /// [`MAX_LOSS_RUN`]).
    lost: u16,
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
            lost: 0,
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
        self.lost = 0;
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
        // A run of losses retina's counters cannot hold: start over, as on
        // any other error, and the packet opens a fresh run.
        let loss = match self.lost.checked_add(loss) {
            Some(lost) if lost <= MAX_LOSS_RUN => loss,
            _ => {
                self.reset();
                0
            }
        };
        self.lost = self.lost.saturating_add(loss);
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
                // The cap also holds for what comes out: AVCC gives every NAL
                // unit a four-byte length, so tiny NAL units grow up to five
                // times over the payloads `pending` counted.
                Ok(CodecItem::VideoFrame(frame)) if frame.data().len() > self.max_frame => {
                    self.pending = 0;
                    self.lost = 0;
                    failure = Some(format!("frame over {} bytes", self.max_frame));
                }
                Ok(CodecItem::AudioFrame(frame)) if frame.data().len() > self.max_frame => {
                    self.pending = 0;
                    self.lost = 0;
                    failure = Some(format!("frame over {} bytes", self.max_frame));
                }
                Ok(CodecItem::VideoFrame(frame)) => {
                    self.pending = 0;
                    self.lost = 0;
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
                    self.lost = 0;
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

    /// Found by the `depacketizers_take_plausible_packets` property: retina
    /// sums the losses of a run of packets in a `u16` (`pkts += meta.loss`,
    /// then `meta.loss + additional_loss`), so a few large sequence gaps at
    /// one timestamp overflowed it: a panic in a debug build and under a
    /// fuzzer, a wrapped count in release.
    #[test]
    fn large_sequence_gaps_never_overflow_the_loss_count() {
        let runs: [(&str, Codec, &[u8], &[u8]); 2] =
            [("h264", Codec::H264, &[0x65], &[0x7c, 0x05]), ("h265", Codec::H265, &[0x26, 0x01], &[0x62, 0x01, 0x13])];
        for (encoding, codec, single, fragment) in runs {
            let mut d = TrackDepacketizer::new(spec("video", encoding, 90_000, None), codec, 1 << 20).unwrap();
            let mut out = Vec::new();
            // The shrunk case, then a longer run of gaps just under half the
            // sequence space, all at one timestamp.
            for (seq, ts, payload) in [(0u16, 0u32, single), (22424, 9000, fragment), (34320, 9000, single)] {
                let _ = d.push(&rtp(seq, ts, false, 96, payload), &mut out);
            }
            let mut seq = 984u16;
            for _ in 0..8 {
                let _ = d.push(&rtp(seq, 9000, false, 96, fragment), &mut out);
                seq = seq.wrapping_add(0x7fff);
            }
            let _ = d.push(&rtp(seq, 0, false, 96, single), &mut out);
        }
    }

    /// The cap holds for what comes out, not only for what went in: an
    /// access unit of one-byte NAL units is five times larger in AVCC (a
    /// four-byte length each) than the payloads that carried it.
    #[test]
    fn a_frame_over_the_cap_never_comes_out() {
        let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 4096).unwrap();
        let mut out = Vec::new();
        let mut refused = false;
        for seq in 0..3001u16 {
            let ts = if seq < 3000 { 0 } else { 3000 };
            refused |= d.push(&rtp(seq, ts, false, 96, &[0x41]), &mut out).is_err();
        }
        assert!(out.iter().all(|f| f.data.len() <= 4096), "{:?}", out.iter().map(|f| f.data.len()).collect::<Vec<_>>());
        assert!(refused);
    }

    /// x264 at `-level:v 4.1` (go2rtc's H.264 template) encoding 2304x1296:
    /// 11664 macroblocks over level 4.1's 8192. h264-reader 0.9.0 refused the
    /// SPS, from the SDP and in band, and no frame ever came out: the bench's
    /// B03 had no picture. The patched copy (third_party/README.md) reads it.
    #[test]
    fn an_sps_over_its_declared_level_is_read() {
        const SPS: [u8; 24] = [
            0x67, 0x64, 0x00, 0x29, 0xac, 0xb4, 0x01, 0x20, 0x05, 0x1d, 0x80, 0x88, 0x00, 0x00, 0x03, 0x00, 0x38, 0x00,
            0x00, 0x06, 0x44, 0x78, 0xc1, 0x95,
        ];
        const PPS: [u8; 4] = [0x68, 0xef, 0x0f, 0xcb];
        let fmtp = "packetization-mode=1;sprop-parameter-sets=Z2QAKay0ASAFHYCIAAADADgAAAZEeMGV,aO8Pyw==;profile-level-id=640029";
        let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, Some(fmtp)), Codec::H264, 1 << 20).unwrap();
        assert!(matches!(d.params(), CodecParams::H264 { ref sps, .. } if sps[..] == SPS[..]), "{:?}", d.params());
        let mut out = Vec::new();
        d.push(&rtp(0, 0, false, 96, &SPS), &mut out).unwrap();
        d.push(&rtp(1, 0, false, 96, &PPS), &mut out).unwrap();
        d.push(&rtp(2, 0, true, 96, &[0x65, 0x88, 0x84, 0x00, 0x33]), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].keyframe);
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
