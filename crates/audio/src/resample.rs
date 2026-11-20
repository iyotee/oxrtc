//! A windowed-sinc polyphase resampler.
//!
//! The ratio `to / from` is reduced to `up / down`; output sample `n` sits at
//! input time `n * down / up`, and is the input convolved with a low-pass
//! sinc there: `fc * sinc(fc * t)`, `fc` the lower of the two Nyquist
//! frequencies (times [`ROLLOFF`]), cut to [`ZERO_CROSSINGS`] of its zeros on
//! each side by a Kaiser window (beta [`KAISER_BETA`], about 80 dB of
//! stop-band). One filter per fractional position (`up` of them), computed
//! once; each tap set is normalised to a gain of 1. The output is aligned on
//! the input: its first sample is the input's first instant, with no delay to
//! correct, and it lags the input by half the filter (2.25 ms from 8 kHz,
//! 2.1 ms from 48 kHz) only in when it can be computed.
//!
//! Checked against FFmpeg's `aresample` (`tests/resample.rs`).

/// The share of the lower Nyquist frequency that is kept.
pub const ROLLOFF: f64 = 0.94;
/// Zeros of the sinc kept on each side of its centre, at the lower rate.
pub const ZERO_CROSSINGS: f64 = 16.0;
/// The Kaiser window's shape.
pub const KAISER_BETA: f64 = 8.6;
/// The largest `up` or `down` a ratio may reduce to: 8 kHz to 44.1 kHz is
/// 441/80.
pub const MAX_PHASES: u32 = 1000;
/// The rates accepted, in Hz.
pub const RATES: std::ops::RangeInclusive<u32> = 1000..=192_000;
/// Input samples consumed before the buffer is moved down.
const COMPACT_AFTER: usize = 4096;

/// A pair of rates this resampler does not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateError {
    /// The input rate.
    pub from: u32,
    /// The output rate.
    pub to: u32,
}

impl std::fmt::Display for RateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot resample {} Hz to {} Hz", self.from, self.to)
    }
}

impl std::error::Error for RateError {}

/// A resampler from one rate to another, one channel, fed in pieces.
#[derive(Debug, Clone)]
pub struct Resampler {
    up: usize,
    down: usize,
    /// Taps per output sample: twice `half`.
    taps: usize,
    half: usize,
    /// `up` filters of `taps` taps each, phase after phase. Empty: the rates
    /// are equal and samples pass as they are.
    bank: Vec<f32>,
    /// Input not yet behind every output that needs it; starts with
    /// `half - 1` zeros, the input before its first sample.
    history: Vec<f32>,
    /// The next output's position: `history[at]` plus `phase / up`.
    at: usize,
    phase: usize,
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The zeroth-order modified Bessel function of the first kind.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..64 {
        let half = x / (2.0 * f64::from(k));
        term *= half * half;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Resampler {
    /// A resampler from `from` Hz to `to` Hz.
    ///
    /// # Errors
    ///
    /// A rate outside [`RATES`], or a ratio that does not reduce to
    /// [`MAX_PHASES`] or fewer.
    pub fn new(from: u32, to: u32) -> Result<Self, RateError> {
        if !RATES.contains(&from) || !RATES.contains(&to) {
            return Err(RateError { from, to });
        }
        let common = gcd(from, to);
        let (up, down) = (to / common, from / common);
        if up > MAX_PHASES || down > MAX_PHASES {
            return Err(RateError { from, to });
        }
        let (up, down) = (up as usize, down as usize);
        if up == down {
            return Ok(Self { up, down, taps: 0, half: 0, bank: Vec::new(), history: Vec::new(), at: 0, phase: 0 });
        }
        let cutoff = (up as f64 / down as f64).min(1.0) * ROLLOFF;
        let width = ZERO_CROSSINGS / cutoff;
        let half = width.ceil() as usize + 1;
        let taps = 2 * half;
        let norm = bessel_i0(KAISER_BETA);
        let mut bank = Vec::with_capacity(up * taps);
        for phase in 0..up {
            let frac = phase as f64 / up as f64;
            let start = bank.len();
            for tap in 0..taps {
                let t = frac + (half as f64 - 1.0) - tap as f64;
                let u = t / width;
                let window = if u.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) / norm };
                let x = std::f64::consts::PI * cutoff * t;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { x.sin() / x };
                bank.push(cutoff * sinc * window);
            }
            let sum: f64 = bank.get(start..).map_or(1.0, |f| f.iter().sum());
            if let Some(filter) = bank.get_mut(start..) {
                for tap in filter {
                    *tap /= sum;
                }
            }
        }
        let bank = bank.into_iter().map(|t| t as f32).collect();
        let mut resampler = Self { up, down, taps, half, bank, history: Vec::new(), at: 0, phase: 0 };
        resampler.reset();
        Ok(resampler)
    }

    /// Whether samples pass as they are.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.bank.is_empty()
    }

    /// Forgets the input so far: the next sample is the start of a new signal.
    pub fn reset(&mut self) {
        self.history.clear();
        self.history.resize(self.half.saturating_sub(1), 0.0);
        self.at = self.half.saturating_sub(1);
        self.phase = 0;
    }

    /// Resamples `input`, the samples that follow the previous call's, and
    /// appends what can be computed so far to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        self.history.extend(input.iter().map(|&s| f32::from(s)));
        while self.at + self.half < self.history.len() {
            let first = self.at + 1 - self.half;
            let filter = self.bank.get(self.phase * self.taps..(self.phase + 1) * self.taps);
            let window = self.history.get(first..first + self.taps);
            let value = match (filter, window) {
                (Some(filter), Some(window)) => filter.iter().zip(window).map(|(h, x)| h * x).sum::<f32>(),
                _ => 0.0,
            };
            out.push(value.round().clamp(-32768.0, 32767.0) as i16);
            self.phase += self.down;
            self.at += self.phase / self.up;
            self.phase %= self.up;
        }
        let consumed = (self.at + 1).saturating_sub(self.half);
        if consumed >= COMPACT_AFTER {
            self.history.drain(..consumed);
            self.at -= consumed;
        }
    }

    /// How many output samples `input_samples` more input will give at most:
    /// for reserving room.
    #[must_use]
    pub fn output_bound(&self, input_samples: usize) -> usize {
        input_samples * self.up / self.down.max(1) + 2
    }

    /// Input samples held, the bound of what this resampler keeps: at most
    /// [`COMPACT_AFTER`] consumed ones, the filter, and the last call's input.
    #[must_use]
    pub fn held(&self) -> usize {
        self.history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_are_reduced_and_bounded() {
        let r = Resampler::new(8000, 48_000).unwrap();
        assert_eq!((r.up, r.down), (6, 1));
        let r = Resampler::new(44_100, 48_000).unwrap();
        assert_eq!((r.up, r.down), (160, 147));
        assert!(Resampler::new(8000, 8000).unwrap().is_identity());
        assert!(Resampler::new(8000, 44_100).is_ok());
        assert!(Resampler::new(999, 8000).is_err());
        assert!(Resampler::new(8000, 200_000).is_err());
        assert_eq!(Resampler::new(8001, 48_000).unwrap_err(), RateError { from: 8001, to: 48_000 });
    }

    /// A constant comes out as the same constant, once past the start.
    #[test]
    fn a_constant_keeps_its_level() {
        for (from, to) in [(8000, 48_000), (48_000, 8000), (44_100, 48_000), (16_000, 44_100)] {
            let mut r = Resampler::new(from, to).unwrap();
            let mut out = Vec::new();
            r.process(&vec![10_000; from as usize / 10], &mut out);
            let settled = out.get(out.len() / 2..).unwrap();
            assert!(settled.iter().all(|&s| (s - 10_000).abs() <= 1), "{from} to {to}: {:?}", &settled[..8]);
        }
    }

    /// Fed in pieces of any size, the output is the same as in one piece.
    #[test]
    fn pieces_do_not_change_the_output() {
        let input: Vec<i16> = (0..9000).map(|i| ((i * 37 % 2000) - 1000) as i16).collect();
        let mut whole = Vec::new();
        Resampler::new(8000, 44_100).unwrap().process(&input, &mut whole);
        let mut pieces = Vec::new();
        let mut r = Resampler::new(8000, 44_100).unwrap();
        for chunk in input.chunks(7) {
            r.process(chunk, &mut pieces);
        }
        assert_eq!(whole, pieces);
        assert!(r.held() < COMPACT_AFTER + r.taps + 8, "{} held", r.held());
        // The output keeps pace with the input: 44.1/8 times as many, less the filter's lag.
        assert!((whole.len() as i64 - 9000 * 441 / 80).abs() < 120, "{}", whole.len());
    }

    #[test]
    fn a_reset_starts_a_new_signal() {
        let mut r = Resampler::new(16_000, 48_000).unwrap();
        let mut first = Vec::new();
        r.process(&[5000; 400], &mut first);
        r.reset();
        let mut second = Vec::new();
        r.process(&[5000; 400], &mut second);
        assert_eq!(first, second);
    }
}
