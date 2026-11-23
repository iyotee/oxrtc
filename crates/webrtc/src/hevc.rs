//! H.265 to a browser that offers it (W7): the camera's own picture goes out
//! only when one of the offer's H.265 payload types takes the camera's
//! profile, tier and level; otherwise the stream's H.264 (openIVR's
//! transcode) goes instead, and str0m is told the numbers of what is sent so
//! that the answer says them.
//!
//! The rule is the core's, the one MSE follows too
//! ([`oxrtc_core::h265_receiver`], D41, D44); this module reads an offer's
//! payload type into it and says what str0m is told. RFC 7798's
//! `profile-id`, `tier-flag` and `level-id` are the SPS's
//! `general_profile_idc`, `general_tier_flag` and `general_level_idc`; a
//! receiver's `level-id` is the highest level it decodes (Chrome 136+ offers
//! 180, level 6.0, with a hardware decoder), and an absent one no ceiling
//! (D41 as amended 2026-11-23).

use oxrtc_core::h265_receiver::{HevcCamera, HevcReceiver};
use str0m::format::PayloadParams;

/// An H.265 profile, tier and level, as RFC 7798 numbers them: what str0m
/// is told to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcPtl {
    /// `profile-id`: 1 Main, 2 Main 10.
    pub profile: u8,
    /// `tier-flag`: 1 for the High tier.
    pub tier: u8,
    /// `level-id`: 30 times the level.
    pub level: u8,
}

/// RFC 7798 7.1: an offer that names none of the three means Main, the Main
/// tier and level 3.1; str0m 0.25 matches such a payload type as that.
pub(crate) const RFC_DEFAULT: HevcPtl = HevcPtl { profile: 1, tier: 0, level: 93 };

/// The levels of ITU-T H.265 table A.8 (str0m takes only these).
const LEVELS: [u8; 13] = [30, 60, 63, 90, 93, 120, 123, 150, 153, 156, 180, 183, 186];

impl HevcPtl {
    /// Whether str0m can be told these numbers as they are (`add_h265`
    /// falls back to Main, Main tier, 3.1 on any other).
    pub(crate) fn is_standard(self) -> bool {
        (1..=11).contains(&self.profile) && self.tier <= 1 && LEVELS.contains(&self.level)
    }
}

/// What one of the offer's H.265 payload types receives, as str0m reads its
/// `fmtp` - str0m keeps the three numbers only when all three are given, and
/// else a lone `profile-id` - with `raw_level`, the `level-id` the payload
/// type's `fmtp` line writes when str0m kept none of the three (a partial
/// `fmtp`, `level-id=150;profile-id=2`): an explicit level is a ceiling, an
/// absent one none (D41). The profile and tier are str0m's whatever a
/// partial line says, since str0m answers such a payload type as Main of the
/// Main tier: a Main 10 camera "taken" by it on the line's word would get an
/// answer with no video (what is left for str0m, ROADMAP D (4)).
pub(crate) fn offered(params: &PayloadParams, raw_level: Option<u8>) -> HevcReceiver {
    let format = &params.spec().format;
    if let Some(ptl) = format.h265_profile_tier_level {
        return HevcReceiver {
            space: 0,
            profile: Some(ptl.profile_id()),
            tier: ptl.tier_flag(),
            level: Some(ptl.level_id()),
        };
    }
    let profile = format.profile_id.and_then(|p| u8::try_from(p).ok()).unwrap_or(RFC_DEFAULT.profile);
    let level = if format.profile_id.is_some() { None } else { raw_level };
    HevcReceiver { space: 0, profile: Some(profile), tier: RFC_DEFAULT.tier, level }
}

/// The numbers str0m is told for a camera of `own` (`None`: its SPS is not
/// known yet) and an offer of `offered`. The first payload type that takes
/// the camera: its profile and tier (the camera's own, or one the camera's
/// picture is compatible with: str0m matches both exactly) at the camera's
/// level, so the answer names what is sent (the payload type's level when
/// the camera's is no standard one). When none takes it, the stream had
/// nothing else to send and the picture goes anyway, on the numbers of the
/// offer's first payload type, which str0m then matches (told the camera's,
/// a Main 10 camera found no Main payload type and the answer carried no
/// video at all). `None` for `enable_h265`.
pub(crate) fn answered(own: Option<&HevcCamera>, offered: &[HevcReceiver]) -> Option<HevcPtl> {
    let told_as = |receiver: &HevcReceiver, level: Option<u8>| HevcPtl {
        profile: receiver.profile.unwrap_or(RFC_DEFAULT.profile),
        tier: receiver.tier,
        level: level.or(receiver.level).unwrap_or(RFC_DEFAULT.level),
    };
    let taker = own.and_then(|own| offered.iter().find(|r| r.takes(own)).map(|r| (own, r)));
    let told = match taker {
        Some((own, receiver)) => {
            let told = told_as(receiver, Some(own.level));
            if told.is_standard() { told } else { told_as(receiver, None) }
        }
        None => told_as(offered.first()?, None),
    };
    told.is_standard().then_some(told)
}

#[cfg(test)]
#[path = "hevc_tests.rs"]
mod tests;
