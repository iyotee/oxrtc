//! The G.722 encoder (ITU-T G.722, 64 kbit/s mode): 16 kHz sound split by
//! the transmit quadrature mirror filter into two sub-bands of 8 kHz, the
//! lower one coded on 6 bits and the upper on 2 by adaptive differential PCM,
//! one byte per pair of input samples.
//!
//! Written from the recommendation's block descriptions (QMF, then for each
//! band SUBTRA, QUANTL/QUANTH, INVQAL/INVQAH, LOGSCL/LOGSCH, SCALEL/SCALEH and
//! the predictor's RECONS, PARREC, UPPOL2, UPPOL1, UPZERO, DELAYA, FILTEP,
//! FILTEZ, PREDIC), its tables, and its arithmetic: 16-bit saturation and
//! arithmetic shifts. RTP carries it at a clock of 8000 (RFC 3551: an error
//! of the original specification kept for compatibility). Equal, bit for
//! bit, to FFmpeg's encoder on tones and on silence, and checked against
//! FFmpeg's `g722` encoder and decoder (`tests/g722.rs`).

/// The transmit QMF's coefficients, h0 to h11 (the filter is symmetric:
/// h23 to h12 are the same).
const QMF: [i32; 12] = [3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11];

/// QUANTL's decision levels (Table 6 of G.722), in the scale factor's units.
const Q6: [i32; 30] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940, 1023, 1121, 1219, 1339,
    1458, 1612, 1765, 1980, 2195, 2557, 2919,
];
/// The 6-bit codes of a negative and a positive difference, by interval.
const ILN: [i32; 31] = [
    0, 63, 62, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4,
];
const ILP: [i32; 31] = [
    0, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41, 40, 39, 38, 37, 36, 35, 34,
    33, 32,
];
/// INVQAL's 4-bit reconstruction levels.
const QM4: [i32; 16] =
    [0, -20456, -12896, -8968, -6288, -4240, -2584, -1200, 20456, 12896, 8968, 6288, 4240, 2584, 1200, 0];
/// LOGSCL's mapping of a 4-bit code to its log factor, and the factors.
const RL42: [usize; 16] = [0, 7, 6, 5, 4, 3, 2, 1, 7, 6, 5, 4, 3, 2, 1, 0];
const WL: [i32; 8] = [-60, -30, 58, 172, 334, 538, 1198, 3042];
/// SCALEL/SCALEH's table of the inverse logarithm.
const ILB: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834, 2896, 2960, 3025,
    3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];
/// The upper band: INVQAH's levels, QUANTH's codes, LOGSCH's factors.
const QM2: [i32; 4] = [-7408, -1616, 7408, 1616];
const IHN: [i32; 3] = [0, 1, 0];
const IHP: [i32; 3] = [0, 3, 2];
const RH2: [usize; 4] = [2, 1, 2, 1];
const WH: [i32; 3] = [0, -214, 798];

/// A value held to 16 bits, as every adder of the recommendation is.
fn sat(value: i32) -> i32 {
    value.clamp(-32768, 32767)
}

/// A table entry; the indexes are bounded by construction, and an out of
/// range one (none can be) reads 0.
fn at<T: Copy + Default>(table: &[T], index: usize) -> T {
    table.get(index).copied().unwrap_or_default()
}

/// The adaptive predictor and scale factor of one sub-band.
#[derive(Debug, Clone)]
struct Band {
    /// The signal estimate, and its pole and zero parts.
    s: i32,
    sp: i32,
    sz: i32,
    /// Reconstructed signals, and partially reconstructed ones, newest first.
    r: [i32; 3],
    p: [i32; 3],
    /// Pole and zero predictor coefficients (index 0 unused).
    a: [i32; 3],
    b: [i32; 7],
    /// Quantized differences, newest first.
    d: [i32; 7],
    /// The log scale factor and the scale factor.
    nb: i32,
    det: i32,
}

impl Band {
    fn new(det: i32) -> Self {
        Self { s: 0, sp: 0, sz: 0, r: [0; 3], p: [0; 3], a: [0; 3], b: [0; 7], d: [0; 7], nb: 0, det }
    }

    /// The scale factor from the log one (SCALEL with `shift` 8, SCALEH 10).
    fn scale(&mut self, shift: i32) {
        let index = ((self.nb >> 6) & 31) as usize;
        let exponent = shift - (self.nb >> 11);
        let wd = if exponent < 0 { at(&ILB, index) << -exponent } else { at(&ILB, index) >> exponent };
        self.det = wd << 2;
    }

    /// The predictor's update with the quantized difference `d` (RECONS to PREDIC).
    fn adapt(&mut self, d: i32) {
        let sign = |v: i32| v >> 15;
        // RECONS, PARREC.
        self.d[0] = d;
        self.r[0] = sat(self.s + d);
        self.p[0] = sat(self.sz + d);
        let (sg0, sg1, sg2) = (sign(self.p[0]), sign(self.p[1]), sign(self.p[2]));
        // UPPOL2: the second pole coefficient.
        let wa1 = sat(self.a[1] << 2);
        let wa2 = (if sg0 == sg1 { -wa1 } else { wa1 }).min(32767);
        let mut a2 = if sg0 == sg2 { 128 } else { -128 };
        a2 += wa2 >> 7;
        a2 += (self.a[2] * 32512) >> 15;
        let a2 = a2.clamp(-12288, 12288);
        // UPPOL1: the first, bounded by the second.
        let mut a1 = sat((if sg0 == sg1 { 192 } else { -192 }) + ((self.a[1] * 32640) >> 15));
        let limit = sat(15360 - a2);
        a1 = a1.clamp(-limit, limit);
        // UPZERO: the six zero coefficients.
        let step = if d == 0 { 0 } else { 128 };
        let mut b = [0; 7];
        for i in 1..7 {
            let same = sign(d) == sign(at(&self.d, i));
            let leak = (at(&self.b, i) * 32640) >> 15;
            if let Some(slot) = b.get_mut(i) {
                *slot = sat(if same { step } else { -step } + leak);
            }
        }
        // DELAYA.
        self.d.copy_within(0..6, 1);
        self.b = b;
        self.r.copy_within(0..2, 1);
        self.p.copy_within(0..2, 1);
        self.a = [0, a1, a2];
        // FILTEP, FILTEZ, PREDIC.
        let pole = |a: i32, r: i32| (a * sat(r + r)) >> 15;
        self.sp = sat(pole(self.a[1], self.r[1]) + pole(self.a[2], self.r[2]));
        let mut sz = 0;
        for i in 1..7 {
            sz += (at(&self.b, i) * sat(at(&self.d, i) * 2)) >> 15;
        }
        self.sz = sat(sz);
        self.s = sat(self.sp + self.sz);
    }

    /// The lower band's 6-bit code of `xl`, and the band adapted to it.
    fn encode_low(&mut self, xl: i32) -> i32 {
        // SUBTRA, QUANTL.
        let el = sat(xl - self.s);
        let magnitude = if el >= 0 { el } else { -(el + 1) };
        let interval = (1..30).find(|&i| magnitude < (at(&Q6, i) * self.det) >> 12).unwrap_or(30);
        let code = if el < 0 { at(&ILN, interval) } else { at(&ILP, interval) };
        // INVQAL: the 4-bit difference the decoder's predictor sees.
        let four = (code >> 2) as usize;
        let dl = (self.det * at(&QM4, four)) >> 15;
        // LOGSCL, SCALEL.
        self.nb = (((self.nb * 127) >> 7) + at(&WL, at(&RL42, four))).clamp(0, 18432);
        self.scale(8);
        self.adapt(dl);
        code
    }

    /// The upper band's 2-bit code of `xh`, and the band adapted to it.
    fn encode_high(&mut self, xh: i32) -> i32 {
        // SUBTRA, QUANTH.
        let eh = sat(xh - self.s);
        let magnitude = if eh >= 0 { eh } else { -(eh + 1) };
        let level = if magnitude >= (564 * self.det) >> 12 { 2 } else { 1 };
        let code = if eh < 0 { at(&IHN, level) } else { at(&IHP, level) };
        // INVQAH, LOGSCH, SCALEH.
        let dh = (self.det * at(&QM2, code as usize)) >> 15;
        self.nb = (((self.nb * 127) >> 7) + at(&WH, at(&RH2, code as usize))).clamp(0, 22528);
        self.scale(10);
        self.adapt(dh);
        code
    }
}

/// A G.722 encoder at 64 kbit/s: 16 kHz samples in, a byte per two out.
#[derive(Debug, Clone)]
pub struct Encoder {
    /// The QMF's delay line, oldest first.
    x: [i32; 24],
    low: Band,
    high: Band,
    /// A sample left over from an odd-length call.
    pending: Option<i16>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// An encoder in the recommendation's reset state.
    #[must_use]
    pub fn new() -> Self {
        Self { x: [0; 24], low: Band::new(32), high: Band::new(8), pending: None }
    }

    /// Encodes `samples` (16 kHz, after the previous call's) and appends one
    /// byte per pair to `out`; an odd sample waits for the next call.
    pub fn encode(&mut self, samples: &[i16], out: &mut Vec<u8>) {
        out.reserve(samples.len() / 2 + 1);
        let mut rest = samples;
        if let Some(first) = self.pending.take() {
            match rest.split_first() {
                Some((&second, tail)) => {
                    out.push(self.encode_pair(first, second));
                    rest = tail;
                }
                None => {
                    self.pending = Some(first);
                    return;
                }
            }
        }
        let (pairs, odd) = rest.as_chunks::<2>();
        for &[a, b] in pairs {
            out.push(self.encode_pair(a, b));
        }
        self.pending = odd.first().copied();
    }

    fn encode_pair(&mut self, first: i16, second: i16) -> u8 {
        // The transmit QMF: the newest two samples in, every other output kept.
        self.x.copy_within(2..24, 0);
        self.x[22] = i32::from(first);
        self.x[23] = i32::from(second);
        let (mut odd, mut even) = (0i32, 0i32);
        for (i, &h) in QMF.iter().enumerate() {
            odd += at(&self.x, 2 * i) * h;
            even += at(&self.x, 2 * i + 1) * at(&QMF, 11 - i);
        }
        let xl = (even + odd) >> 14;
        let xh = (even - odd) >> 14;
        let low = self.low.encode_low(xl);
        let high = self.high.encode_high(xh);
        ((high << 6) | low) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence does not settle on one code: the predictor wanders at the
    /// smallest scale factor, as the recommendation's does (FFmpeg's encoder
    /// writes the same bytes, `tests/g722.rs`). What holds is that it is the
    /// same from one encoder to the next.
    #[test]
    fn silence_is_coded_the_same_every_time() {
        let code = |n: usize| {
            let mut out = Vec::new();
            Encoder::new().encode(&vec![0; n], &mut out);
            out
        };
        assert_eq!(code(640), code(640));
        assert_eq!(code(640).len(), 320);
    }

    #[test]
    fn an_odd_sample_waits_for_the_next_call() {
        let samples: Vec<i16> = (0..101).map(|i| (i * 300 % 8000) as i16).collect();
        let mut whole = Vec::new();
        Encoder::new().encode(&samples, &mut whole);
        let mut split = Vec::new();
        let mut encoder = Encoder::new();
        for chunk in samples.chunks(3) {
            encoder.encode(chunk, &mut split);
        }
        assert_eq!(whole, split);
        assert_eq!(whole.len(), 50);
    }

    #[test]
    fn full_scale_input_never_overflows() {
        let mut encoder = Encoder::new();
        let mut out = Vec::new();
        let square: Vec<i16> = (0..20_000).map(|i| if (i / 7) % 2 == 0 { i16::MAX } else { i16::MIN }).collect();
        encoder.encode(&square, &mut out);
        assert_eq!(out.len(), 10_000);
    }
}
