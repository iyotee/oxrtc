//! The resampler against the ideal and against FFmpeg's `aresample`: two
//! tones inside the band both rates carry, resampled; the output compared
//! with the same tones computed at the new rate, and with FFmpeg's output.

mod common;

use oxrtc_audio::resample::Resampler;

/// Two tones at `rate`, `n` samples, starting at time 0.
fn two_tones(rate: u32, n: usize, low: f64, high: f64) -> Vec<i16> {
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let v = 0.4 * (2.0 * std::f64::consts::PI * low * t).sin()
                + 0.3 * (2.0 * std::f64::consts::PI * high * t + 0.7).sin();
            (v * 32767.0).round() as i16
        })
        .collect()
}

/// The pairs openIVR's cameras and readers need: G.711's 8 kHz to Opus' 48,
/// wide band to narrow, CD rate to 48 kHz, and the others in the list.
const PAIRS: [(u32, u32); 8] = [
    (8000, 48_000),
    (8000, 16_000),
    (16_000, 48_000),
    (48_000, 8000),
    (44_100, 48_000),
    (48_000, 44_100),
    (32_000, 48_000),
    (8000, 44_100),
];

/// `got` against `want` from `from` on (the start, where both filters see
/// the zeros before the signal, is left out), at the best lag in `lags`.
fn best_snr(want: &[i16], got: &[i16], from: usize, lags: std::ops::RangeInclusive<i64>) -> (f64, i64) {
    lags.map(|lag| {
        let start = (from as i64 + lag).max(0) as usize;
        let shifted = got.get(start..).unwrap_or_default();
        let end = want.len().min(from + shifted.len()).saturating_sub(from / 2);
        (common::snr_db(want.get(from..end).unwrap_or_default(), shifted), lag)
    })
    .fold((f64::MIN, 0), |best, now| if now.0 > best.0 { now } else { best })
}

/// Within 60 dB of the ideal at no lag (the output is aligned on the input),
/// and within 45 dB of FFmpeg's, also at no lag: FFmpeg's filter (32 taps a
/// phase, Kaiser beta 9, cutoff 0.97) is a little shorter than this one and
/// passes a little more of the band's edge, so the two differ by that much.
#[test]
fn tones_come_out_as_the_ideal_and_as_ffmpeg_makes_them() {
    let ffmpeg = common::ffmpeg();
    for (from, to) in PAIRS {
        let (low, high) = (440.0, 0.35 * f64::from(from.min(to)));
        let input = two_tones(from, from as usize, low, high);
        let want = two_tones(to, to as usize, low, high);
        let mut got = Vec::new();
        let mut resampler = Resampler::new(from, to).unwrap();
        for chunk in input.chunks(160) {
            resampler.process(chunk, &mut got);
        }
        let skip = to as usize / 20;
        let (ideal, lag) = best_snr(&want, &got, skip, -3..=3);
        eprintln!("{from} -> {to}: {ideal:.1} dB from the ideal at lag {lag}");
        assert_eq!(lag, 0, "{from} -> {to}");
        assert!(ideal > 60.0, "{from} -> {to}: {ideal:.1} dB");
        if let Some(bin) = &ffmpeg {
            let theirs = common::s16le(&common::run(
                bin,
                &["-f", "s16le", "-ar", &from.to_string(), "-ac", "1"],
                &["-af", &format!("aresample={to}"), "-f", "s16le"],
                &common::to_s16le(&input),
            ));
            let (against, lag) = best_snr(&theirs, &got, skip, -3..=3);
            let (theirs_ideal, _) = best_snr(&want, &theirs, skip, 0..=0);
            eprintln!(
                "{from} -> {to}: {against:.1} dB from FFmpeg's at lag {lag} (FFmpeg's {theirs_ideal:.1} dB from the ideal)"
            );
            assert_eq!(lag, 0, "{from} -> {to}");
            assert!(against > 45.0, "{from} -> {to}: {against:.1} dB");
        }
    }
}

/// What is above the lower rate's band is taken out: a tone a quarter above
/// its Nyquist frequency, past the filter's transition (its cutoff is 0.94 of
/// that frequency and the window's transition about a sixth of it either
/// side), resampled down, leaves less than -60 dB. Between 0.94 and 1.1 of
/// the Nyquist frequency the rejection grows from -6 dB to that, as for any
/// filter of this length (FFmpeg's included).
#[test]
fn what_the_lower_rate_cannot_carry_is_filtered_out() {
    for (from, to) in [(48_000, 8000), (16_000, 8000), (48_000, 16_000), (44_100, 16_000)] {
        let tone = f64::from(to) * 0.5 * 1.25;
        let input = two_tones(from, from as usize, tone, tone);
        let mut got = Vec::new();
        Resampler::new(from, to).unwrap().process(&input, &mut got);
        let rms = |s: &[i16]| (s.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt();
        let settled = got.get(got.len() / 4..got.len() * 3 / 4).unwrap_or_default();
        let ratio = 20.0 * (rms(settled) / rms(&input)).log10();
        eprintln!("{from} -> {to}: a {tone:.0} Hz tone at {ratio:.1} dB");
        assert!(ratio < -60.0, "{from} -> {to}: {ratio:.1} dB");
    }
}
