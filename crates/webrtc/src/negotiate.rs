//! The str0m side of an answer: the `Rtc` configuration a subscription
//! calls for, the offer accepted with its panics contained, and the media
//! lines the session then sends on.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::{Codec, MediaKind, TrackInfo};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::config::DtlsCert;
use str0m::crypto::CryptoProvider;
use str0m::media::{Direction, MediaKind as RtcKind, Mid, Pt};
use str0m::net::TcpType;
use str0m::{Candidate, Rtc, RtcConfig};
use tracing::{debug, warn};

use crate::WhepError;
use crate::candidates::LocalCandidates;
use crate::hevc::HevcPtl;
use crate::offer::OfferedCodecs;
use crate::payload::Output;

/// str0m's own payload types for H.265 and its retransmissions (the answer
/// takes the offer's numbers).
const H265_PT: u8 = 102;
const H265_RTX_PT: u8 = 103;

/// A configuration that offers, per media, only the codec the subscription
/// delivers (every offered one when it delivers none). ICE-lite with host
/// candidates only; full ICE as soon as a server-reflexive or relayed
/// candidate is offered, which ICE-lite cannot have (RFC 8445 section 2.5,
/// and str0m refuses them).
pub(crate) fn rtc_config(
    crypto: &Arc<CryptoProvider>,
    cert: &DtlsCert,
    codecs: &OfferedCodecs,
    tracks: &[TrackInfo],
    lite: bool,
) -> RtcConfig {
    let mut config = RtcConfig::new()
        .set_ice_lite(lite)
        .set_crypto_provider(crypto.clone())
        .set_dtls_cert(cert.clone())
        .clear_codecs();
    for (kind, offered) in [(MediaKind::Video, &codecs.video), (MediaKind::Audio, &codecs.audio)] {
        let delivered: Vec<Codec> =
            tracks.iter().filter(|t| t.kind == kind && offered.contains(&t.codec)).map(|t| t.codec).collect();
        let enable = if delivered.is_empty() { offered.clone() } else { delivered };
        for codec in enable {
            config = match codec {
                Codec::H264 => config.enable_h264(true),
                // The camera's own profile, tier and level, so that str0m
                // matches the offer's payload type of that profile (Main 10
                // is not Main) and the answer names what is sent; the
                // offer's own when none of its types takes the camera's
                // (`hevc::answered`).
                Codec::H265 => match crate::hevc::answered(tracks.iter().find_map(HevcPtl::of_track), &codecs.h265) {
                    Some(own) => {
                        let (pt, rtx) = (Pt::new_with_value(H265_PT), Some(Pt::new_with_value(H265_RTX_PT)));
                        config.codec_config().add_h265(pt, rtx, own.profile, own.tier, own.level);
                        config
                    }
                    None => config.enable_h265(true),
                },
                Codec::Opus => config.enable_opus(true, false),
                Codec::Pcma => config.enable_pcma(true, false),
                Codec::Pcmu => config.enable_pcmu(true, false),
                _ => config,
            };
        }
    }
    config
}

/// The header extension id oxrtc gives `playout-delay`; str0m answers with
/// the id the offer gave it (Chromium offers it on its video line).
const PLAYOUT_DELAY_ID: u8 = 12;

/// The playout-delay header extension, accepted from the offer when the
/// session's picture carries it: lip sync off (`Playout::lip_sync`, D30).
pub(crate) fn with_playout(config: RtcConfig, playout: crate::Playout) -> RtcConfig {
    if playout.lip_sync { config } else { config.set_extension(PLAYOUT_DELAY_ID, str0m::rtp::Extension::PlayoutDelay) }
}

/// The str0m candidates of a candidate set, in order of preference.
fn str0m_candidates(local: &LocalCandidates) -> Vec<Candidate> {
    let mut list = Vec::new();
    let mut add = |made: Result<Candidate, str0m::error::IceError>| match made {
        Ok(candidate) => list.push(candidate),
        Err(error) => debug!(%error, "candidate refused"),
    };
    for addr in &local.host {
        add(Candidate::host(*addr, "udp"));
    }
    for (mapped, base) in &local.reflexive {
        add(Candidate::server_reflexive(*mapped, *base, "udp"));
    }
    for relayed in &local.relayed {
        let base = local.host.iter().find(|h| h.is_ipv4() == relayed.is_ipv4()).copied().unwrap_or(*relayed);
        add(Candidate::relayed(*relayed, base, "udp"));
    }
    for addr in &local.tcp {
        add(Candidate::builder().tcp().host(*addr).tcptype(TcpType::Passive).build());
    }
    list
}

/// Builds the session's `Rtc`, adds the local candidates and accepts the
/// offer. str0m asserts on some inconsistent offers (a payload type claimed
/// twice panics in its codec negotiation): the offer is network input, so
/// such a panic is contained here and becomes [`WhepError::BadOffer`].
pub(crate) fn negotiate(
    config: RtcConfig,
    local: &LocalCandidates,
    offer: SdpOffer,
) -> Result<(Rtc, SdpAnswer), WhepError> {
    let candidates = str0m_candidates(local);
    let run = move || {
        let mut rtc = config.build(Instant::now());
        for candidate in candidates {
            let shown = candidate.addr();
            if rtc.add_local_candidate(candidate).is_none() {
                debug!(addr = %shown, "candidate not added");
            }
        }
        let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| WhepError::BadOffer(e.to_string()))?;
        Ok((rtc, answer))
    };
    std::panic::catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|_| {
        warn!("an offer made str0m panic; refused");
        Err(WhepError::BadOffer("inconsistent offer (payload types)".into()))
    })
}

/// The negotiated media lines this server sends on, with their payload types.
pub(crate) fn outputs(rtc: &mut Rtc, mids: &[Mid]) -> Vec<Output> {
    let mut list = Vec::new();
    for mid in mids {
        let Some(media) = rtc.media(*mid) else { continue };
        if media.disabled() || !matches!(media.direction(), Direction::SendOnly | Direction::SendRecv) {
            continue;
        }
        let kind = match media.kind() {
            RtcKind::Video => MediaKind::Video,
            RtcKind::Audio => MediaKind::Audio,
        };
        let params = rtc.writer(*mid).map(|w| w.payload_params().cloned().collect()).unwrap_or_default();
        list.push(Output { mid: *mid, kind, params });
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offers a hostile peer could send, after they pass the parser: the
    /// answer must be an error or an answer, never a panic.
    #[test]
    fn mangled_offers_never_panic() {
        use str0m::media::MediaKind as Kind;
        let config = RtcConfig::new().clear_codecs().enable_h264(true).enable_h265(true).enable_opus(true, false);
        let good = crate::offer::tests::offer(config, &[Kind::Video, Kind::Audio, Kind::Video]);
        let mut variants = vec![
            good.replace("a=mid:0", "a=mid:1"),
            good.replace("a=ice-ufrag:", "a=x-ufrag:"),
            good.replace("a=ice-pwd:", "a=x-pwd:"),
            good.replace("a=fingerprint:", "a=x-fingerprint:"),
            good.replace("a=setup:actpass", "a=setup:passive"),
            good.replace("a=rtcp-mux", ""),
            good.replace("a=recvonly", "a=sendonly"),
            good.replace("a=recvonly", "a=inactive"),
            good.replace("a=rtpmap:", "a=rtpmap:1"),
            good.replace("a=fmtp:", "a=fmtp:9"),
            good.replace("profile-level-id=", "profile-level-id=zz"),
            good.replace(" 9 UDP/TLS/RTP/SAVPF", " 0 UDP/TLS/RTP/SAVPF"),
            good.replace("UDP/TLS/RTP/SAVPF", "RTP/AVP"),
            good.replace("a=group:BUNDLE", "a=group:LS"),
            good.replace("a=ssrc", "a=x-ssrc"),
        ];
        // An fmtp line is optional in SDP, and dropping one (for H.264 at
        // least) is what makes str0m 0.25 claim a payload type twice.
        let lines: Vec<&str> = good.split("\r\n").collect();
        for (skip, line) in lines.iter().enumerate() {
            if line.starts_with("a=fmtp:") || line.starts_with("a=rtpmap:") {
                let mut v: Vec<&str> = lines.clone();
                v.remove(skip);
                variants.push(v.join("\r\n"));
            }
        }
        let candidates = LocalCandidates {
            host: vec![std::net::SocketAddr::from(([127, 0, 0, 1], 5000))],
            ..LocalCandidates::default()
        };
        // One certificate for every attempt, as the server does.
        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let cert = crypto.dtls_provider.generate_certificate().unwrap();
        let mut contained = 0;
        for sdp in variants {
            let Ok(offer) = SdpOffer::from_sdp_string(&sdp) else { continue };
            let config = RtcConfig::new()
                .set_ice_lite(true)
                .set_crypto_provider(crypto.clone())
                .set_dtls_cert(cert.clone())
                .clear_codecs()
                .enable_h264(true)
                .enable_opus(true, false);
            if let Err(WhepError::BadOffer(reason)) = negotiate(config, &candidates, offer)
                && reason.starts_with("inconsistent offer")
            {
                contained += 1;
            }
        }
        // At least one variant does make str0m panic: the guard is exercised.
        assert!(contained > 0, "no variant reached str0m's assertion; the test no longer proves the guard");
    }
}
