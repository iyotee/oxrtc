//! H.265 to a browser that offers it (W7): the camera's own picture goes out
//! only when one of the offer's H.265 payload types takes the camera's
//! profile, tier and level; otherwise the stream's H.264 (openIVR's
//! transcode) goes instead, and str0m is told the camera's own profile, tier
//! and level so that the answer says what is sent.
//!
//! RFC 7798's `profile-id`, `tier-flag` and `level-id` are the SPS's
//! `general_profile_idc`, `general_tier_flag` and `general_level_idc`. For a
//! receiver, `level-id` is the highest level it decodes (Chrome 136+ and Edge
//! offer 180, level 6.0, with a hardware decoder); profile and tier are
//! symmetric (RFC 7798 7.2.2), and str0m 0.25 matches them exactly.

use oxrtc_core::h265::sps_profile_tier_level;
use oxrtc_core::{Codec, CodecParams, MediaKind, TrackInfo};
use str0m::format::PayloadParams;

/// An H.265 profile, tier and level, as RFC 7798 numbers them.
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
/// tier and level 3.1.
pub(crate) const RFC_DEFAULT: HevcPtl = HevcPtl { profile: 1, tier: 0, level: 93 };

/// The levels of ITU-T H.265 table A.8 (str0m takes only these).
const LEVELS: [u8; 13] = [30, 60, 63, 90, 93, 120, 123, 150, 153, 156, 180, 183, 186];

impl HevcPtl {
    /// What one of the offer's H.265 payload types receives, as str0m read
    /// its `fmtp` (it keeps the three only when all three are given; a lone
    /// `profile-id` keeps the RFC's tier and level).
    pub(crate) fn offered(params: &PayloadParams) -> Self {
        let format = &params.spec().format;
        if let Some(ptl) = format.h265_profile_tier_level {
            return Self { profile: ptl.profile_id(), tier: ptl.tier_flag(), level: ptl.level_id() };
        }
        let profile = format.profile_id.and_then(|p| u8::try_from(p).ok()).unwrap_or(RFC_DEFAULT.profile);
        Self { profile, ..RFC_DEFAULT }
    }

    /// The profile, tier and level an H.265 track's SPS codes, or `None`
    /// when the track is not H.265, has no SPS yet, or is of a profile space
    /// other than the standard's.
    pub(crate) fn of_track(track: &TrackInfo) -> Option<Self> {
        let CodecParams::H265 { sps, .. } = &track.params else { return None };
        if track.codec != Codec::H265 {
            return None;
        }
        let ptl = sps_profile_tier_level(sps).filter(|ptl| ptl.space == 0)?;
        Some(Self { profile: ptl.profile, tier: u8::from(ptl.tier), level: ptl.level })
    }

    /// Whether a receiver of `self` decodes a stream of `stream`: the same
    /// profile and tier, a level no higher.
    pub(crate) fn takes(self, stream: Self) -> bool {
        self.profile == stream.profile && self.tier == stream.tier && stream.level <= self.level
    }

    /// Whether str0m can be told these numbers as they are (`add_h265`
    /// falls back to Main, Main tier, 3.1 on any other).
    pub(crate) fn is_standard(self) -> bool {
        (1..=11).contains(&self.profile) && self.tier <= 1 && LEVELS.contains(&self.level)
    }
}

/// The first H.265 video track of `tracks` none of `offered` takes, with
/// its profile, tier and level: such a viewer is better served the stream's
/// H.264. A track whose SPS is not known yet is taken on trust, as before.
pub(crate) fn refused(offered: &[HevcPtl], tracks: &[TrackInfo]) -> Option<HevcPtl> {
    tracks
        .iter()
        .filter(|t| t.kind == MediaKind::Video)
        .filter_map(HevcPtl::of_track)
        .find(|stream| !offered.iter().any(|o| o.takes(*stream)))
}

#[cfg(test)]
#[path = "hevc_tests.rs"]
mod tests;
