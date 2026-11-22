//! H.265: the profile, tier and level and the picture size from the SPS
//! (ITU-T H.265, 7.3.2.2 and 7.3.3), `hvcC` (ISO/IEC 14496-15, 8.3.3.1) and the
//! codec string (ISO/IEC 14496-15, annex E).

use oxrtc_core::bits::{BitReader, Exhausted, rbsp};
pub(crate) use oxrtc_core::h265::ProfileTierLevel;
use oxrtc_core::h265::{MAX_SUB_LAYERS, profile_tier_level};

use crate::Mp4Error;

/// What the sample entry takes from an SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sps {
    pub ptl: ProfileTierLevel,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub width: u32,
    pub height: u32,
}

/// Reads an SPS NAL unit, its two header bytes included.
pub(crate) fn parse_sps(nal: &[u8]) -> Result<Sps, Mp4Error> {
    let bad = |_: Exhausted| Mp4Error::invalid("H.265 SPS", "ends before its picture size");
    if nal.first().map(|b| (b >> 1) & 0x3f) != Some(33) {
        return Err(Mp4Error::invalid("H.265 SPS", "not an SPS NAL unit"));
    }
    let data = rbsp(nal.get(2..).unwrap_or_default());
    let mut r = BitReader::new(&data);
    r.skip(4).map_err(bad)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u8(3).map_err(bad)?;
    if max_sub_layers_minus1 >= MAX_SUB_LAYERS {
        return Err(Mp4Error::invalid("H.265 SPS", "more than 7 sub-layers"));
    }
    let temporal_id_nesting = r.bit().map_err(bad)?;
    let ptl = profile_tier_level(&mut r, max_sub_layers_minus1).map_err(bad)?;
    r.ue().map_err(bad)?; // sps_seq_parameter_set_id
    let chroma_format = r.ue().map_err(bad)?;
    if chroma_format > 3 {
        return Err(Mp4Error::invalid("H.265 SPS", "a chroma format over 3"));
    }
    if chroma_format == 3 {
        r.bit().map_err(bad)?; // separate_colour_plane_flag
    }
    let width = u64::from(r.ue().map_err(bad)?);
    let height = u64::from(r.ue().map_err(bad)?);
    let mut window = [0u64; 4];
    if r.bit().map_err(bad)? {
        for side in &mut window {
            *side = u64::from(r.ue().map_err(bad)?);
        }
    }
    let bit_depth_luma = r.ue().map_err(bad)?.saturating_add(8);
    let bit_depth_chroma = r.ue().map_err(bad)?.saturating_add(8);
    let (sub_width, sub_height) = match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let [left, right, top, bottom] = window;
    let fit = |v: Option<u64>| v.and_then(|v| u32::try_from(v).ok()).filter(|&v| v > 0);
    let (Some(width), Some(height)) =
        (fit(width.checked_sub(sub_width * (left + right))), fit(height.checked_sub(sub_height * (top + bottom))))
    else {
        return Err(Mp4Error::invalid("H.265 SPS", "a conformance window larger than the picture"));
    };
    let depth = |v: u32| {
        u8::try_from(v)
            .ok()
            .filter(|v| (8..=16).contains(v))
            .ok_or(Mp4Error::invalid("H.265 SPS", "a bit depth out of range"))
    };
    Ok(Sps {
        ptl,
        max_sub_layers: max_sub_layers_minus1 + 1,
        temporal_id_nesting,
        chroma_format: chroma_format as u8,
        bit_depth_luma: depth(bit_depth_luma)?,
        bit_depth_chroma: depth(bit_depth_chroma)?,
        width,
        height,
    })
}

/// The `hvcC` box: the SPS's profile, tier and level, then one array per
/// parameter set type. `complete` marks the arrays complete (no set of their
/// type in band): what `hvc1` requires (ISO/IEC 14496-15, 8.4.1); `hev1`
/// leaves them incomplete, which is what the core's frames are, since
/// `oxrtc-depacketize` puts the sets in band ahead of every keyframe.
pub(crate) fn hvcc(vps: &[u8], sps_nal: &[u8], pps: &[u8], sps: &Sps, complete: bool) -> Result<Vec<u8>, Mp4Error> {
    let ptl = &sps.ptl;
    let mut w = crate::boxes::BoxWriter::with_capacity(64 + vps.len() + sps_nal.len() + pps.len());
    let open = w.start(b"hvcC");
    w.u8(1); // configurationVersion
    w.u8((ptl.space << 6) | (u8::from(ptl.tier) << 5) | (ptl.profile & 0x1f));
    w.u32(ptl.compatibility);
    w.bytes(&ptl.constraints);
    w.u8(ptl.level);
    w.u16(0xf000); // min_spatial_segmentation_idc = 0
    w.u8(0xfc); // parallelismType = 0
    w.u8(0xfc | (sps.chroma_format & 3));
    w.u8(0xf8 | (sps.bit_depth_luma.saturating_sub(8) & 7));
    w.u8(0xf8 | (sps.bit_depth_chroma.saturating_sub(8) & 7));
    w.u16(0); // avgFrameRate
    // constantFrameRate 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne 3.
    w.u8(((sps.max_sub_layers & 7) << 3) | (u8::from(sps.temporal_id_nesting) << 2) | 3);
    w.u8(3); // numOfArrays
    for (kind, nal) in [(32u8, vps), (33, sps_nal), (34, pps)] {
        let len = u16::try_from(nal.len()).map_err(|_| Mp4Error::invalid("H.265 parameter set", "over 65535 bytes"))?;
        w.u8((u8::from(complete) << 7) | kind); // array_completeness, reserved 0, type
        w.u16(1);
        w.u16(len);
        w.bytes(nal);
    }
    w.end(open)?;
    Ok(w.into_inner())
}

/// `hvc1.<space><profile>.<compatibility>.<tier><level>[.<constraint byte>...]`
/// (or `hev1.`): the compatibility flags in reverse bit order, the constraint
/// bytes up to the last one that is not zero.
pub(crate) fn codec_string(tag: &str, ptl: &ProfileTierLevel) -> String {
    let space = match ptl.space {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let mut out = format!(
        "{tag}.{space}{}.{:X}.{}{}",
        ptl.profile,
        ptl.compatibility.reverse_bits(),
        if ptl.tier { 'H' } else { 'L' },
        ptl.level
    );
    let used = ptl.constraints.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    for byte in ptl.constraints.iter().take(used) {
        out.push_str(&format!(".{byte:02X}"));
    }
    out
}

#[cfg(test)]
#[path = "h265_tests.rs"]
pub(crate) mod tests;
