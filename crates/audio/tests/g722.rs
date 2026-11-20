//! The G.722 encoder against FFmpeg's: the same input coded by both, both
//! bit streams decoded by FFmpeg's decoder.

mod common;

use oxrtc_audio::g722::Encoder;

fn ours(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::new();
    Encoder::new().encode(samples, &mut out);
    out
}

fn decode(bin: &std::path::PathBuf, coded: &[u8]) -> Vec<i16> {
    common::s16le(&common::run(bin, &["-f", "g722"], &["-f", "s16le", "-ar", "16000", "-ac", "1"], coded))
}

/// Every code equal to FFmpeg's (its encoder without trellis search codes as
/// the recommendation does), and the decoded sound as close to the input:
/// about 30 dB on these tones, the codec's own.
#[test]
fn tones_are_coded_as_ffmpeg_codes_them() {
    let Some(bin) = common::ffmpeg() else { return };
    let input = common::tones(16_000, 2.0, 6000.0);
    let reference = common::run(
        &bin,
        &["-f", "s16le", "-ar", "16000", "-ac", "1"],
        &["-c:a", "g722", "-f", "g722"],
        &common::to_s16le(&input),
    );
    let mine = ours(&input);
    assert_eq!(mine.len(), reference.len());
    let same = mine.iter().zip(&reference).filter(|(a, b)| a == b).count();
    let share = same as f64 / mine.len() as f64;
    let ours_decoded = decode(&bin, &mine);
    let theirs_decoded = decode(&bin, &reference);
    // The codec's delay: the two QMFs, 23 samples, compared at the best of a few.
    let best = |decoded: &[i16]| {
        (0..48).map(|lag| common::snr_db(&input, decoded.get(lag..).unwrap_or_default())).fold(f64::MIN, f64::max)
    };
    let (snr_ours, snr_theirs) = (best(&ours_decoded), best(&theirs_decoded));
    eprintln!("codes equal {:.1} %, SNR ours {snr_ours:.1} dB, FFmpeg's {snr_theirs:.1} dB", share * 100.0);
    assert!(snr_ours > 20.0, "{snr_ours}");
    assert!(snr_ours > snr_theirs - 0.5, "ours {snr_ours} dB, FFmpeg's {snr_theirs} dB");
    assert_eq!(same, mine.len(), "{:.2} % of the codes equal", share * 100.0);
}

#[test]
fn silence_is_coded_as_ffmpeg_codes_it() {
    let Some(bin) = common::ffmpeg() else { return };
    let input = vec![0i16; 3200];
    let reference = common::run(
        &bin,
        &["-f", "s16le", "-ar", "16000", "-ac", "1"],
        &["-c:a", "g722", "-f", "g722"],
        &common::to_s16le(&input),
    );
    assert_eq!(ours(&input), reference);
}
