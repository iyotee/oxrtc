//! WHEP offers from the network, under proptest: the stable counterpart of
//! the `whep_offer` fuzz target. An offer is the one SDP a remote browser
//! controls entirely; reading it must answer or refuse, never panic.

use std::time::Instant;

use oxrtc_core::Codec;
use oxrtc_webrtc::OfferedCodecs;
use proptest::prelude::*;
use str0m::RtcConfig;
use str0m::media::{Direction, MediaKind};

/// A browser-like offer with the codecs picked by `mask`.
fn offer(mask: u8, video: bool, audio: bool) -> String {
    let mut config = RtcConfig::new().clear_codecs();
    config = config.enable_h264(mask & 1 != 0).enable_h265(mask & 2 != 0).enable_vp8(mask & 4 != 0);
    config = config.enable_opus(mask & 8 != 0, false);
    config = config.enable_pcma(mask & 16 != 0, false).enable_pcmu(mask & 32 != 0, false);
    let mut rtc = config.build(Instant::now());
    let mut api = rtc.sdp_api();
    if video {
        api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    }
    if audio {
        api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    }
    match api.apply() {
        Some((offer, _pending)) => offer.to_sdp_string(),
        None => String::new(),
    }
}

/// What a successful read may contain: this server's codecs, each once, in
/// its order of preference.
fn well_formed(codecs: &OfferedCodecs) -> bool {
    let ordered = |got: &[Codec], order: &[Codec]| {
        let positions: Option<Vec<usize>> = got.iter().map(|c| order.iter().position(|o| o == c)).collect();
        positions.is_some_and(|p| p.windows(2).all(|w| matches!(w, [a, b] if a < b)))
    };
    ordered(&codecs.video, &[Codec::H265, Codec::H264])
        && ordered(&codecs.audio, &[Codec::Opus, Codec::Pcma, Codec::Pcmu])
}

/// The generator's offers, unedited, are read: the mutated property starts
/// from offers that get past the parser, not from ones it already refuses.
#[test]
fn unedited_offers_are_read() {
    let codecs = OfferedCodecs::from_sdp(&offer(0b1001, true, true)).unwrap();
    assert_eq!((codecs.video.as_slice(), codecs.audio.as_slice()), (&[Codec::H264][..], &[Codec::Opus][..]));
    let codecs = OfferedCodecs::from_sdp(&offer(0xff, true, true)).unwrap();
    assert!(well_formed(&codecs) && codecs.video.len() == 2 && codecs.audio.len() == 3, "{codecs:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_text_is_answered_or_refused(text in "[ -~\r\n]{0,2000}") {
        if let Ok(codecs) = OfferedCodecs::from_sdp(&text) {
            prop_assert!(well_formed(&codecs), "{codecs:?}");
        }
    }

    /// Real offers with lines replaced, dropped, duplicated or cut short.
    #[test]
    fn mutated_offers_are_answered_or_refused(
        mask in any::<u8>(),
        video in any::<bool>(),
        audio in any::<bool>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), 0u8..4, "[ -~]{0,80}"), 0..6),
    ) {
        let mut lines: Vec<String> = offer(mask, video, audio).split("\r\n").map(str::to_owned).collect();
        for (at, op, text) in edits {
            if lines.is_empty() {
                break;
            }
            let i = at.index(lines.len());
            match op {
                0 => lines[i] = text,
                1 => {
                    lines.remove(i);
                }
                2 => {
                    let line = lines[i].clone();
                    lines.insert(i, line);
                }
                _ => {
                    let cut = at.index(lines[i].len() + 1);
                    let cut = (0..=cut).rev().find(|c| lines[i].is_char_boundary(*c)).unwrap_or(0);
                    lines[i].truncate(cut);
                }
            }
        }
        if let Ok(codecs) = OfferedCodecs::from_sdp(&lines.join("\r\n")) {
            prop_assert!(well_formed(&codecs), "{codecs:?}");
        }
    }
}
