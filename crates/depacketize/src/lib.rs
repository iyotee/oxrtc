//! RTP to frames, with retina's depacketizers and a byte cap on the access unit
//! in progress: the one depacketize path of the project (decision D4), for the
//! cameras (`oxrtc-rtsp-client`) as for the publications of the local
//! transcoders (`oxrtc-rtsp-server`).
//!
//! retina's own demuxer caps neither the size of the access unit it builds nor
//! its number of pieces: a camera (or whoever speaks as one) that never ends an
//! access unit grows it until the stall timeout. Here it is cut at `max_frame`
//! bytes, counted on what goes in and again on what comes out (R11).
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
pub struct DepacketizerSpec {
    /// `video` or `audio`.
    pub media: String,
    /// Lower-case encoding name of the `rtpmap` (`h264`, `mpeg4-generic`...).
    pub encoding: String,
    /// The RTP clock rate.
    pub clock_rate: NonZeroU32,
    /// Audio channels, if announced.
    pub channels: Option<NonZeroU16>,
    /// The `fmtp` parameters, if any.
    pub fmtp: Option<String>,
}

/// A frame out of the depacketizer.
#[derive(Debug, Clone)]
pub struct Depacketized {
    /// Timestamp since the first packet, in clock-rate units.
    pub pts: i64,
    /// Whether decoding can start here.
    pub keyframe: bool,
    /// AVCC for H.264/H.265, the raw frame otherwise.
    pub data: Bytes,
    /// Whether this frame changed the codec parameters ([`TrackDepacketizer::params`]
    /// answers differently from here on).
    pub new_parameters: bool,
}

/// One announced track's depacketizer.
pub struct TrackDepacketizer {
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
    pub fn new(spec: DepacketizerSpec, codec: Codec, max_frame: usize) -> Result<Self, String> {
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
    pub fn params(&self) -> CodecParams {
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
    pub fn push(&mut self, packet: &[u8], out: &mut Vec<Depacketized>) -> Result<(), String> {
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
        // The cap is judged after the packet is pushed: a camera that sets no
        // marker bit ends a unit only when the next timestamp shows, and the
        // packet that shows it is then the new unit's, not an addition to the
        // one it ends (below). It costs one packet of memory over the cap.
        self.pending = self.pending.saturating_add(payload.len());
        let elapsed = timestamp.elapsed();
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
                    self.frame_ended(frame.timestamp().elapsed() == elapsed, payload.len());
                    failure = Some(format!("frame over {} bytes", self.max_frame));
                }
                Ok(CodecItem::AudioFrame(frame)) if frame.data().len() > self.max_frame => {
                    self.pending = 0;
                    self.lost = 0;
                    failure = Some(format!("frame over {} bytes", self.max_frame));
                }
                Ok(CodecItem::VideoFrame(frame)) => {
                    self.frame_ended(frame.timestamp().elapsed() == elapsed, payload.len());
                    let pts = frame.timestamp().elapsed();
                    let rap = frame.is_random_access_point();
                    let new_parameters = frame.has_new_parameters();
                    let data = Bytes::from(frame.into_data());
                    let keyframe = rap
                        || match self.codec {
                            Codec::H264 => h264_is_keyframe(&data),
                            Codec::H265 => h265_is_keyframe(&data),
                            _ => false,
                        };
                    out.push(Depacketized { pts, keyframe, data, new_parameters });
                }
                Ok(CodecItem::AudioFrame(frame)) => {
                    self.pending = 0;
                    self.lost = 0;
                    let pts = frame.timestamp().elapsed();
                    out.push(Depacketized {
                        pts,
                        keyframe: true,
                        data: Bytes::copy_from_slice(frame.data()),
                        new_parameters: false,
                    });
                }
                Ok(_) => {}
                Err(error) => failure = Some(format!("{error:?}")),
            }
        }
        if failure.is_none() && self.pending > self.max_frame {
            self.reset();
            failure = Some(format!("frame over {} bytes", self.max_frame));
        }
        failure.map_or(Ok(()), Err)
    }

    /// A frame came out: what is pending restarts. A frame of this packet's
    /// timestamp has this packet in it; a frame of an earlier one was ended
    /// by it, and the packet is the first of the next unit.
    fn frame_ended(&mut self, with_this_packet: bool, packet_bytes: usize) {
        self.pending = if with_this_packet { 0 } else { packet_bytes };
        self.lost = 0;
    }
}

#[cfg(test)]
mod tests;
