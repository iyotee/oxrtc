//! Reading a browser's offer: which codecs it can receive, and the [`Want`]
//! they make.

use oxrtc_core::want::preferred;
use oxrtc_core::{Codec, MediaKind, Want};
use str0m::change::SdpOffer;
use str0m::format::Codec as RtcCodec;
use str0m::media::Direction;

use crate::WhepError;
use crate::hevc::HevcPtl;

/// Longest accepted offer (the HTTP API caps request bodies at the same size).
pub const MAX_OFFER_BYTES: usize = 64 * 1024;

/// H.265 payload types of an offer kept, at most (a browser offers two).
const MAX_H265_TYPES: usize = 32;

/// The codecs an offer is able to receive, among those this server sends
/// (H.264, H.265, Opus, G.711 at 8 kHz), in the order of preference every
/// egress shares ([`oxrtc_core::PREFERENCE`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfferedCodecs {
    /// Video codecs, H.265 first.
    pub video: Vec<Codec>,
    /// Audio codecs, Opus first, then PCMA and PCMU.
    pub audio: Vec<Codec>,
    /// What each of the offer's H.265 payload types receives, in its order
    /// (Chrome and Edge offer Main and Main 10 at level 6.0).
    pub h265: Vec<HevcPtl>,
}

impl OfferedCodecs {
    /// Reads the codecs of an SDP offer.
    pub fn from_sdp(sdp: &str) -> Result<Self, WhepError> {
        parse_offer(sdp).map(|(_, codecs)| codecs)
    }

    /// Whether the offer lists nothing this server can send.
    pub fn is_empty(&self) -> bool {
        self.video.is_empty() && self.audio.is_empty()
    }

    /// The same offer without its H.265: what a viewer is subscribed to when
    /// none of its H.265 payload types takes the camera's (`hevc.rs`).
    pub(crate) fn without_h265(&self) -> Self {
        Self {
            video: self.video.iter().copied().filter(|c| *c != Codec::H265).collect(),
            h265: Vec::new(),
            ..self.clone()
        }
    }

    /// What to subscribe to: each media the offer can receive, in this
    /// server's order of preference.
    pub fn want(&self) -> Want {
        Want::preferred(&[self.video.as_slice(), self.audio.as_slice()].concat())
    }
}

/// Parses an offer, and lists the codecs its receiving media lines accept.
/// G.711 is only taken at 8 kHz, the rate browsers decode.
pub fn parse_offer(sdp: &str) -> Result<(SdpOffer, OfferedCodecs), WhepError> {
    if sdp.len() > MAX_OFFER_BYTES {
        return Err(WhepError::BadOffer(format!("offer longer than {MAX_OFFER_BYTES} bytes")));
    }
    // str0m's parse error quotes its parser's state, a memory address of the
    // server among it (`PointerOffset(0x...)`), and it reaches the client: it
    // is not repeated.
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| WhepError::BadOffer("the SDP cannot be parsed".into()))?;
    let mut seen: Vec<Codec> = Vec::new();
    let mut h265: Vec<HevcPtl> = Vec::new();
    for line in offer.media_lines.iter() {
        if line.disabled || !matches!(line.direction(), Direction::RecvOnly | Direction::SendRecv) {
            continue;
        }
        for params in line.rtp_params() {
            let spec = params.spec();
            let codec = match spec.codec {
                RtcCodec::H264 => Codec::H264,
                RtcCodec::H265 => Codec::H265,
                RtcCodec::Opus => Codec::Opus,
                RtcCodec::PCMA if spec.clock_rate.get() == 8000 => Codec::Pcma,
                RtcCodec::PCMU if spec.clock_rate.get() == 8000 => Codec::Pcmu,
                _ => continue,
            };
            if !seen.contains(&codec) {
                seen.push(codec);
            }
            if codec == Codec::H265 && h265.len() < MAX_H265_TYPES {
                h265.push(HevcPtl::offered(&params));
            }
        }
    }
    let codecs =
        OfferedCodecs { video: preferred(MediaKind::Video, &seen), audio: preferred(MediaKind::Audio, &seen), h265 };
    Ok((offer, codecs))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use str0m::RtcConfig;
    use str0m::media::MediaKind as RtcKind;

    use oxrtc_core::MediaWant;

    use super::*;

    /// An offer as a browser would make it, from a str0m client.
    pub(crate) fn offer(config: RtcConfig, kinds: &[RtcKind]) -> String {
        let mut rtc = config.build(Instant::now());
        let mut api = rtc.sdp_api();
        for kind in kinds {
            api.add_media(*kind, Direction::RecvOnly, None, None, None);
        }
        let (offer, _pending) = api.apply().unwrap();
        offer.to_sdp_string()
    }

    fn video_audio() -> [RtcKind; 2] {
        [RtcKind::Video, RtcKind::Audio]
    }

    #[test]
    fn h264_only_wants_h264() {
        let config = RtcConfig::new().clear_codecs().enable_h264(true).enable_opus(true, false);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &video_audio())).unwrap();
        assert_eq!(codecs.video, vec![Codec::H264]);
        let want = codecs.want();
        assert_eq!(want.video, MediaWant::OneOf(vec![Codec::H264]));
        assert_eq!(want.audio, MediaWant::OneOf(vec![Codec::Opus]));
    }

    #[test]
    fn h265_comes_first_when_offered() {
        let config = RtcConfig::new().clear_codecs().enable_h264(true).enable_h265(true);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &[RtcKind::Video])).unwrap();
        assert_eq!(codecs.want().video, MediaWant::OneOf(vec![Codec::H265, Codec::H264]));
        assert_eq!(codecs.want().audio, MediaWant::Skip);
    }

    #[test]
    fn audio_only_opus() {
        let config = RtcConfig::new().clear_codecs().enable_opus(true, false);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &[RtcKind::Audio])).unwrap();
        let want = codecs.want();
        assert_eq!((want.video, want.audio), (MediaWant::Skip, MediaWant::OneOf(vec![Codec::Opus])));
    }

    #[test]
    fn g711_follows_opus() {
        let config =
            RtcConfig::new().clear_codecs().enable_pcmu(true, false).enable_pcma(true, false).enable_opus(true, false);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &[RtcKind::Audio])).unwrap();
        assert_eq!(codecs.audio, vec![Codec::Opus, Codec::Pcma, Codec::Pcmu]);
    }

    #[test]
    fn nothing_supported_is_empty() {
        let config = RtcConfig::new().clear_codecs().enable_vp8(true).enable_vp9(true);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &[RtcKind::Video])).unwrap();
        assert!(codecs.is_empty());
        assert!(codecs.want().is_empty());
    }

    #[test]
    fn refuses_garbage_and_oversized_offers() {
        assert!(matches!(OfferedCodecs::from_sdp("hello"), Err(WhepError::BadOffer(_))));
        // The reason reaches the client: no address of the server's memory.
        for garbage in ["hello", "not sdp", "v=0\r\nm=video x"] {
            let Err(WhepError::BadOffer(reason)) = OfferedCodecs::from_sdp(garbage) else { panic!("{garbage}") };
            assert_eq!(reason, "the SDP cannot be parsed", "{garbage}");
        }
        let huge = "v=0\r\n".repeat(MAX_OFFER_BYTES);
        assert!(matches!(OfferedCodecs::from_sdp(&huge), Err(WhepError::BadOffer(_))));
    }
}
