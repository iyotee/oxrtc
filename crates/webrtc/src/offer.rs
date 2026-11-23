//! Reading a browser's offer: which codecs it can receive, and the [`Want`]
//! they make.

use oxrtc_core::want::preferred;
use oxrtc_core::{Codec, MediaKind, MediaWant, Want};
use str0m::change::SdpOffer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, MediaKind as RtcKind};

use crate::WhepError;
use oxrtc_core::h265_receiver::HevcReceiver;

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
    /// (Chrome 157 offers Main and Main 10 at level 6.0).
    pub h265: Vec<HevcReceiver>,
    /// Every media codec the offer's receiving lines list, sent by this
    /// server or not, as go2rtc names them (`video:VP8`, `audio:OPUS`), at
    /// most [`MAX_NAMED`]: what a refusal says the offer asked for.
    pub named: Vec<String>,
    /// Whether a receiving video line is offered.
    pub video_line: bool,
}

/// Codec names an offer keeps for its refusal, at most.
pub const MAX_NAMED: usize = 24;

impl OfferedCodecs {
    /// Reads the codecs of an SDP offer.
    pub fn from_sdp(sdp: &str) -> Result<Self, WhepError> {
        parse_offer(sdp).map(|(_, codecs)| codecs)
    }

    /// Whether the offer lists nothing this server can send.
    pub fn is_empty(&self) -> bool {
        self.video.is_empty() && self.audio.is_empty()
    }

    /// Whether the offer has a receiving video line on which it lists no
    /// video codec this server sends (VP8, VP9 or AV1 alone): it is answered
    /// only when the stream has no picture to send it (D43).
    pub fn video_unusable(&self) -> bool {
        self.video_line && self.video.is_empty()
    }

    /// What to subscribe to for an answer: [`Self::want`], except that an
    /// offer whose video is unusable asks for the stream's picture in any
    /// codec, to learn whether the stream has one (then the offer is
    /// refused, never answered with the sound alone) or not (then the sound
    /// is answered and the video line rejected).
    pub(crate) fn answer_want(&self) -> Want {
        let want = self.want();
        if self.video_unusable() { Want { video: MediaWant::Any, ..want } } else { want }
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
    let mut h265: Vec<HevcReceiver> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    let mut video_line = false;
    for (index, line) in offer.media_lines.iter().enumerate() {
        if line.disabled || !matches!(line.direction(), Direction::RecvOnly | Direction::SendRecv) {
            continue;
        }
        let kind = if line.typ == RtcKind::Video.into() {
            "video"
        } else if line.typ == RtcKind::Audio.into() {
            "audio"
        } else {
            continue;
        };
        video_line |= kind == "video";
        for params in line.rtp_params() {
            let spec = params.spec();
            if let Some(name) = media_codec_name(spec.codec) {
                let name = format!("{kind}:{name}");
                if named.len() < MAX_NAMED && !named.contains(&name) {
                    named.push(name);
                }
            }
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
                let raw = if params.spec().format.h265_profile_tier_level.is_none() {
                    written_level_id(sdp, index, *params.pt())
                } else {
                    None
                };
                h265.push(crate::hevc::offered(&params, raw));
            }
        }
    }
    let codecs = OfferedCodecs {
        video: preferred(MediaKind::Video, &seen),
        audio: preferred(MediaKind::Audio, &seen),
        h265,
        named,
        video_line,
    };
    Ok((offer, codecs))
}

/// The `level-id` that the `a=fmtp` line of payload type `pt` writes in the
/// `index`th media section of `sdp` (what str0m drops when the line does not
/// give all three of `profile-id`, `tier-flag` and `level-id`): a decimal
/// number of a byte, its key in any case; `None` when the line or the key is
/// absent or the value is no such number.
fn written_level_id(sdp: &str, index: usize, pt: u8) -> Option<u8> {
    let section = sdp.split("\nm=").nth(index.checked_add(1)?)?;
    let prefix = format!("a=fmtp:{pt} ");
    let line = section.lines().map(str::trim).find_map(|line| line.strip_prefix(prefix.as_str()))?;
    line.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        let value = value.trim();
        let digits = !value.is_empty() && value.len() <= 3 && value.bytes().all(|b| b.is_ascii_digit());
        (key.trim().eq_ignore_ascii_case("level-id") && digits).then(|| value.parse().ok()).flatten()
    })
}

/// A media codec's name as go2rtc prints it (`H264`, `VP8`, `OPUS`), or
/// `None` for what carries no media of its own (retransmissions, redundancy,
/// comfort noise, DTMF, what str0m does not know).
fn media_codec_name(codec: RtcCodec) -> Option<String> {
    match codec {
        RtcCodec::Rtx | RtcCodec::Red | RtcCodec::CN | RtcCodec::Tele | RtcCodec::Null | RtcCodec::Unknown => None,
        known => Some(known.to_string().to_ascii_uppercase()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use str0m::RtcConfig;

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
    fn names_what_each_receiving_line_lists_and_whether_its_video_is_usable() {
        let config = RtcConfig::new().clear_codecs().enable_vp8(true).enable_opus(true, false);
        let codecs = OfferedCodecs::from_sdp(&offer(config, &video_audio())).unwrap();
        assert_eq!(codecs.named, vec!["video:VP8".to_owned(), "audio:OPUS".to_owned()]);
        assert!(codecs.video_line && codecs.video_unusable());
        // The stream's picture in any codec, to learn whether it has one.
        let want = codecs.answer_want();
        assert_eq!((want.video, want.audio), (MediaWant::Any, MediaWant::OneOf(vec![Codec::Opus])));
        // H.264 offered: usable, the want unchanged.
        let codecs =
            OfferedCodecs::from_sdp(&offer(RtcConfig::new().clear_codecs().enable_h264(true), &[RtcKind::Video]))
                .unwrap();
        assert!(!codecs.video_unusable());
        assert_eq!(codecs.answer_want(), codecs.want());
        // No video line at all: nothing is unusable.
        let codecs = OfferedCodecs::from_sdp(&offer(
            RtcConfig::new().clear_codecs().enable_opus(true, false),
            &[RtcKind::Audio],
        ))
        .unwrap();
        assert!(!codecs.video_line && !codecs.video_unusable());
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
