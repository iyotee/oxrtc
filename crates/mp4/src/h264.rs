//! H.264: the SPS fields the sample entry needs (ITU-T H.264, 7.3.2.1.1),
//! `avcC` (ISO/IEC 14496-15, 5.3.3.1) and the RFC 6381 codec string.

use crate::Mp4Error;
use oxrtc_core::bits::{BitReader, Exhausted, rbsp};

/// Most entries of the picture order count cycle (H.264 bounds it at 255).
const MAX_POC_CYCLE: u32 = 255;

/// What the sample entry takes from a sequence parameter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sps {
    pub profile: u8,
    pub constraints: u8,
    pub level: u8,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub width: u32,
    pub height: u32,
}

/// Profiles whose SPS carries the chroma format and the bit depths.
fn is_high(profile: u8) -> bool {
    matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

/// Reads an SPS NAL unit, header byte included.
pub(crate) fn parse_sps(nal: &[u8]) -> Result<Sps, Mp4Error> {
    let bad = |_: Exhausted| Mp4Error::invalid("H.264 SPS", "ends before its picture size");
    if nal.first().map(|b| b & 0x1f) != Some(7) {
        return Err(Mp4Error::invalid("H.264 SPS", "not an SPS NAL unit"));
    }
    let data = rbsp(nal.get(1..).unwrap_or_default());
    let mut r = BitReader::new(&data);
    let profile = r.u8(8).map_err(bad)?;
    let constraints = r.u8(8).map_err(bad)?;
    let level = r.u8(8).map_err(bad)?;
    r.ue().map_err(bad)?; // seq_parameter_set_id
    let (mut chroma_format, mut bit_depth_luma, mut bit_depth_chroma) = (1u32, 8u32, 8u32);
    if is_high(profile) {
        chroma_format = r.ue().map_err(bad)?;
        if chroma_format == 3 {
            r.bit().map_err(bad)?; // separate_colour_plane_flag
        }
        bit_depth_luma = r.ue().map_err(bad)?.saturating_add(8);
        bit_depth_chroma = r.ue().map_err(bad)?.saturating_add(8);
        r.bit().map_err(bad)?; // qpprime_y_zero_transform_bypass_flag
        if r.bit().map_err(bad)? {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit().map_err(bad)? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 }).map_err(bad)?;
                }
            }
        }
    }
    r.ue().map_err(bad)?; // log2_max_frame_num_minus4
    match r.ue().map_err(bad)? {
        0 => {
            r.ue().map_err(bad)?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit().map_err(bad)?;
            r.se().map_err(bad)?;
            r.se().map_err(bad)?;
            let cycle = r.ue().map_err(bad)?;
            if cycle > MAX_POC_CYCLE {
                return Err(Mp4Error::invalid("H.264 SPS", "a picture order count cycle over 255"));
            }
            for _ in 0..cycle {
                r.se().map_err(bad)?;
            }
        }
        _ => {}
    }
    r.ue().map_err(bad)?; // max_num_ref_frames
    r.bit().map_err(bad)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = u64::from(r.ue().map_err(bad)?) + 1;
    let height_units = u64::from(r.ue().map_err(bad)?) + 1;
    let frame_mbs_only = r.bit().map_err(bad)?;
    if !frame_mbs_only {
        r.bit().map_err(bad)?; // mb_adaptive_frame_field_flag
    }
    r.bit().map_err(bad)?; // direct_8x8_inference_flag
    let mut crop = [0u64; 4];
    if r.bit().map_err(bad)? {
        for side in &mut crop {
            *side = u64::from(r.ue().map_err(bad)?);
        }
    }
    let [left, right, top, bottom] = crop;
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let (crop_x, crop_y) = match chroma_format {
        0 => (1, field_factor),
        1 => (2, 2 * field_factor),
        2 => (2, field_factor),
        _ => (1, field_factor),
    };
    let width = (width_mbs * 16).checked_sub(crop_x * (left + right));
    let height = (height_units * 16 * field_factor).checked_sub(crop_y * (top + bottom));
    let fit = |v: Option<u64>| v.and_then(|v| u32::try_from(v).ok()).filter(|&v| v > 0);
    let (Some(width), Some(height)) = (fit(width), fit(height)) else {
        return Err(Mp4Error::invalid("H.264 SPS", "a cropping larger than the picture"));
    };
    let narrow = |v: u32| {
        u8::try_from(v).map_err(|_| Mp4Error::invalid("H.264 SPS", "a bit depth or chroma format out of range"))
    };
    Ok(Sps {
        profile,
        constraints,
        level,
        chroma_format: narrow(chroma_format)?,
        bit_depth_luma: narrow(bit_depth_luma)?,
        bit_depth_chroma: narrow(bit_depth_chroma)?,
        width,
        height,
    })
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Result<(), Exhausted> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// The `avcC` box of one SPS and one PPS, with lengths on four bytes.
pub(crate) fn avcc(sps_nal: &[u8], pps_nal: &[u8], sps: &Sps) -> Result<Vec<u8>, Mp4Error> {
    let too_long = |_| Mp4Error::invalid("H.264 parameter set", "over 65535 bytes");
    let sps_len = u16::try_from(sps_nal.len()).map_err(too_long)?;
    let pps_len = u16::try_from(pps_nal.len()).map_err(too_long)?;
    let mut w = crate::boxes::BoxWriter::with_capacity(32 + sps_nal.len() + pps_nal.len());
    let open = w.start(b"avcC");
    w.u8(1); // configurationVersion
    w.u8(sps.profile);
    w.u8(sps.constraints);
    w.u8(sps.level);
    w.u8(0xfc | 3); // lengthSizeMinusOne = 3
    w.u8(0xe0 | 1); // one SPS
    w.u16(sps_len);
    w.bytes(sps_nal);
    w.u8(1); // one PPS
    w.u16(pps_len);
    w.bytes(pps_nal);
    if matches!(sps.profile, 100 | 110 | 122 | 144) {
        w.u8(0xfc | (sps.chroma_format & 3));
        w.u8(0xf8 | (sps.bit_depth_luma.saturating_sub(8) & 7));
        w.u8(0xf8 | (sps.bit_depth_chroma.saturating_sub(8) & 7));
        w.u8(0); // numOfSequenceParameterSetExt
    }
    w.end(open)?;
    Ok(w.into_inner())
}

/// `avc1.PPCCLL`: profile, constraint flags and level, in hexadecimal.
pub(crate) fn codec_string(sps: &Sps) -> String {
    format!("avc1.{:02x}{:02x}{:02x}", sps.profile, sps.constraints, sps.level)
}

#[cfg(test)]
#[path = "h264_tests.rs"]
pub(crate) mod tests;
