//! Whether a receiver of H.265 decodes a camera's picture: one rule for every
//! egress that can send the camera's own H.265 or the stream's H.264 in its
//! place (WebRTC since W7, MSE since M5 item D; D41, D44).
//!
//! A receiver says what it decodes in one of two notations, both read here
//! into a [`HevcReceiver`]: a WebRTC offer's payload type (RFC 7798
//! `profile-id`, `tier-flag`, `level-id`, read by `oxrtc-webrtc` from str0m's
//! parse) and an MSE codec string (ISO/IEC 14496-15 annex E,
//! `hvc1.1.6.L153.B0`, [`HevcReceiver::from_codec_string`]). A camera is what
//! its SPS codes ([`crate::h265::sps_profile_tier_level`]).
//!
//! The rule ([`HevcReceiver::takes`]): the same profile space; the receiver's
//! profile, or one the camera's compatibility flags say its picture conforms
//! to too (a Main picture flagged Main 10 compatible plays on a Main 10
//! decoder, as every libx265 Main stream is); a tier no higher; a level no
//! higher than the receiver's when it names one. A level the receiver does
//! not name is no ceiling (D41 as amended 2026-11-23): RFC 7798 reads an
//! absent `level-id` as 3.1, which would send the transcode to every real
//! camera for a browser that writes none, where go2rtc 1.9.15 and MediaMTX
//! 1.21.3 check no level at all.

use crate::h265::{ProfileTierLevel, sps_profile_tier_level};
use crate::media::{Codec, CodecParams, MediaKind, TrackInfo};

/// What one receiver of H.265 decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HevcReceiver {
    /// `general_profile_space` (0 in every profile of the standard).
    pub space: u8,
    /// `general_profile_idc` (1 Main, 2 Main 10), `None` for any (an MSE
    /// `hvc1` with nothing after it).
    pub profile: Option<u8>,
    /// `general_tier_flag`: 1 for the High tier, which takes the Main tier's
    /// pictures too.
    pub tier: u8,
    /// `general_level_idc` (30 times the level), the highest it decodes;
    /// `None` when it names none, which is no ceiling.
    pub level: Option<u8>,
}

/// A camera's picture as a receiver must take it: its SPS's general
/// profile, tier and level and its compatibility flags.
pub type HevcCamera = ProfileTierLevel;

/// Longest codec string read, in bytes (`hvc1.A31.FFFFFFFF.H255.FF.FF.FF.FF.FF.FF` is 40).
pub const MAX_CODEC_STRING: usize = 64;

impl HevcReceiver {
    /// A receiver that names nothing: it takes every camera.
    pub const ANY: Self = Self { space: 0, profile: None, tier: 1, level: None };

    /// Whether it decodes `camera`'s picture.
    pub fn takes(self, camera: &HevcCamera) -> bool {
        let profile = match self.profile {
            None => true,
            Some(p) => camera.profile == p || compatible(camera, p),
        };
        camera.space == self.space
            && profile
            && u8::from(camera.tier) <= self.tier
            && self.level.is_none_or(|ceiling| camera.level <= ceiling)
    }

    /// Reads an MSE codec string of H.265 (ISO/IEC 14496-15 E.3): `hvc1` or
    /// `hev1`, then, each optional and in this order, the profile space
    /// (none, `A`, `B`, `C`) and `general_profile_idc`; the 32 compatibility
    /// flags in hexadecimal, in reverse bit order; the tier (`L` or `H`) and
    /// `general_level_idc`; up to six constraint bytes in hexadecimal. The
    /// compatibility flags and the constraint bytes are read to check the
    /// string and say nothing of what the receiver decodes. `None` for what
    /// is not such a string (another codec, a malformed element, more than
    /// [`MAX_CODEC_STRING`] bytes); a string that stops early names less (no
    /// level: no ceiling; no tier: the Main tier; the family alone:
    /// [`Self::ANY`]).
    pub fn from_codec_string(name: &str) -> Option<Self> {
        if name.len() > MAX_CODEC_STRING {
            return None;
        }
        let mut parts = name.split('.');
        let family = parts.next()?;
        if !family.eq_ignore_ascii_case("hvc1") && !family.eq_ignore_ascii_case("hev1") {
            return None;
        }
        let mut receiver = Self { space: 0, profile: None, tier: 0, level: None };
        let Some(profile) = parts.next() else { return Some(Self::ANY) };
        {
            let (space, idc) = match profile.as_bytes().first() {
                Some(b'A' | b'a') => (1, profile.get(1..)?),
                Some(b'B' | b'b') => (2, profile.get(1..)?),
                Some(b'C' | b'c') => (3, profile.get(1..)?),
                _ => (0, profile),
            };
            let idc = decimal(idc)?;
            if idc > 31 {
                return None;
            }
            receiver.space = space;
            receiver.profile = Some(idc);
        }
        if let Some(flags) = parts.next() {
            hex(flags, 8)?;
        }
        if let Some(tier_level) = parts.next() {
            let tier = match tier_level.as_bytes().first() {
                Some(b'L' | b'l') => 0,
                Some(b'H' | b'h') => 1,
                _ => return None,
            };
            receiver.tier = tier;
            receiver.level = Some(decimal(tier_level.get(1..)?)?);
        }
        let mut constraints = 0;
        for byte in parts {
            hex(byte, 2)?;
            constraints += 1;
            if constraints > 6 {
                return None;
            }
        }
        Some(receiver)
    }
}

/// Whether `camera`'s compatibility flags say its picture conforms to
/// `profile` too (`general_profile_compatibility_flag[profile]`, the first
/// flag the most significant bit).
fn compatible(camera: &HevcCamera, profile: u8) -> bool {
    profile < 32 && camera.compatibility & (0x8000_0000u32 >> profile) != 0
}

/// A decimal number of one to three digits that fits a byte.
fn decimal(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 3 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A hexadecimal number of one to `digits` digits.
fn hex(text: &str, digits: usize) -> Option<u32> {
    if text.is_empty() || text.len() > digits || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

/// The camera's picture an H.265 track carries, once its SPS is known and of
/// the standard's profile space; `None` otherwise (such a track is taken on
/// trust, as before W7).
pub fn camera_of(track: &TrackInfo) -> Option<HevcCamera> {
    let CodecParams::H265 { sps, .. } = &track.params else { return None };
    if track.codec != Codec::H265 {
        return None;
    }
    sps_profile_tier_level(sps).filter(|camera| camera.space == 0)
}

/// The first H.265 video track of `tracks` none of `receivers` takes: such a
/// reader is better served the stream's H.264. A track whose SPS is not
/// known yet is taken on trust.
pub fn refused(receivers: &[HevcReceiver], tracks: &[TrackInfo]) -> Option<HevcCamera> {
    tracks
        .iter()
        .filter(|t| t.kind == MediaKind::Video)
        .filter_map(camera_of)
        .find(|camera| !receivers.iter().any(|r| r.takes(camera)))
}

#[cfg(test)]
#[path = "h265_receiver_tests.rs"]
mod tests;
