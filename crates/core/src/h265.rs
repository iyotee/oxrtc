//! H.265's profile, tier and level, as an SPS codes them (ITU-T H.265,
//! 7.3.2.2 and 7.3.3): what an MP4 sample entry and its codec string carry
//! (`oxrtc-mp4`), and what a WebRTC answer is negotiated on (RFC 7798's
//! `profile-id`, `tier-flag` and `level-id` are these numbers).

use crate::bits::{BitReader, Exhausted, rbsp};

/// `profile_tier_level`'s general part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileTierLevel {
    /// `general_profile_space` (0 in every profile of the standard).
    pub space: u8,
    /// `general_tier_flag`: the High tier.
    pub tier: bool,
    /// `general_profile_idc`: 1 Main, 2 Main 10, 3 Main Still Picture, 4 the
    /// range extensions.
    pub profile: u8,
    /// `general_profile_compatibility_flag[32]`, the first one most significant.
    pub compatibility: u32,
    /// The 48 bits of constraint flags, in the order they are coded.
    pub constraints: [u8; 6],
    /// `general_level_idc`: 30 times the level (5.1 is 153).
    pub level: u8,
}

/// The most sub-layers an SPS declares (`sps_max_sub_layers_minus1` is at most 6).
pub const MAX_SUB_LAYERS: u8 = 7;

/// Reads `profile_tier_level(1, max_sub_layers_minus1)`, the sub-layers'
/// parts passed over.
pub fn profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u8) -> Result<ProfileTierLevel, Exhausted> {
    let space = r.u8(2)?;
    let tier = r.bit()?;
    let profile = r.u8(5)?;
    let compatibility = u32::try_from(r.bits(32)?).map_err(|_| Exhausted)?;
    let mut constraints = [0u8; 6];
    for byte in &mut constraints {
        *byte = r.u8(8)?;
    }
    let level = r.u8(8)?;
    let mut present = [(false, false); 8];
    for flags in present.iter_mut().take(usize::from(max_sub_layers_minus1)) {
        *flags = (r.bit()?, r.bit()?);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?; // reserved_zero_2bits
        }
    }
    for &(profile_present, level_present) in present.iter().take(usize::from(max_sub_layers_minus1)) {
        if profile_present {
            r.skip(88)?;
        }
        if level_present {
            r.skip(8)?;
        }
    }
    Ok(ProfileTierLevel { space, tier, profile, compatibility, constraints, level })
}

/// The general profile, tier and level of an SPS NAL unit (its two header
/// bytes included, no start code), or `None` for what is not an SPS or ends
/// before them.
pub fn sps_profile_tier_level(nal: &[u8]) -> Option<ProfileTierLevel> {
    if nal.first().map(|b| (b >> 1) & 0x3f) != Some(33) {
        return None;
    }
    let data = rbsp(nal.get(2..).unwrap_or_default());
    let mut r = BitReader::new(&data);
    r.skip(4).ok()?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u8(3).ok()?;
    if max_sub_layers_minus1 >= MAX_SUB_LAYERS {
        return None;
    }
    r.skip(1).ok()?; // sps_temporal_id_nesting_flag
    profile_tier_level(&mut r, max_sub_layers_minus1).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// libx265's (FFmpeg 8.0.1) SPS of a 1920x1080 Main stream at level 4,
    /// `hvc1.1.6.L120.90` to FFmpeg; its compatibility flags are `60 00 00 00`,
    /// coded `60 00 00 03 00`.
    const MAIN_L4: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";
    /// libx265's Main 10, 640x360, level 2.1 (`hvc1.2.4.L63.90`).
    const MAIN10_L21: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

    #[test]
    fn reads_the_general_profile_tier_and_level_through_emulation_prevention() {
        let ptl = sps_profile_tier_level(&hex(MAIN_L4)).unwrap();
        assert_eq!((ptl.space, ptl.tier, ptl.profile, ptl.level), (0, false, 1, 120));
        assert_eq!(ptl.compatibility, 0x6000_0000);
        assert_eq!(ptl.constraints, [0x90, 0, 0, 0, 0, 0]);
        let ten = sps_profile_tier_level(&hex(MAIN10_L21)).unwrap();
        assert_eq!((ten.tier, ten.profile, ten.level), (false, 2, 63));
    }

    #[test]
    fn what_is_not_a_whole_sps_head_is_none() {
        assert_eq!(sps_profile_tier_level(&[]), None);
        // A PPS (type 34) is not an SPS.
        assert_eq!(sps_profile_tier_level(&[0x44, 0x01, 0xc1]), None);
        // An SPS cut inside its profile, tier and level.
        assert_eq!(sps_profile_tier_level(hex(MAIN_L4).get(..10).unwrap()), None);
        // Eight sub-layers is more than the standard allows.
        assert_eq!(sps_profile_tier_level(&[0x42, 0x01, 0x0e]), None);
    }

    proptest::proptest! {
        /// Whatever a camera sends as its SPS is read without a panic, and a
        /// whole SPS cut anywhere reads as nothing or as the whole one does.
        #[test]
        fn any_sps_bytes_are_read_or_refused(body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
                                             cut in 0usize..48) {
            let mut nal = vec![0x42, 0x01];
            nal.extend(body);
            let _ = sps_profile_tier_level(&nal);
            let whole = hex(MAIN_L4);
            let read = sps_profile_tier_level(whole.get(..cut.min(whole.len())).unwrap());
            proptest::prop_assert!(read.is_none() || read == sps_profile_tier_level(&whole));
        }
    }
}
