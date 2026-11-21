//! A bit reader over a parameter set's RBSP, for the few fields the sample
//! entries need. Every read is checked: past the end is an error, never a
//! panic or a guess.

/// Why a bit read failed: the set ended, or an Exp-Golomb code was longer
/// than 32 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exhausted;

/// The NAL unit's payload with its emulation prevention bytes removed: every
/// `00 00 03` becomes `00 00`.
pub(crate) fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Reads bits most significant first.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    at: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub(crate) fn bit(&mut self) -> Result<bool, Exhausted> {
        let byte = self.data.get(self.at / 8).ok_or(Exhausted)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(bit == 1)
    }

    /// `n` bits (at most 64) as an unsigned number.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u64, Exhausted> {
        if n > 64 {
            return Err(Exhausted);
        }
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value)
    }

    pub(crate) fn u8(&mut self, n: u32) -> Result<u8, Exhausted> {
        u8::try_from(self.bits(n.min(8))?).map_err(|_| Exhausted)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), Exhausted> {
        let to = self.at.checked_add(n).ok_or(Exhausted)?;
        if to > self.data.len().saturating_mul(8) {
            return Err(Exhausted);
        }
        self.at = to;
        Ok(())
    }

    /// An unsigned Exp-Golomb code, `ue(v)`.
    pub(crate) fn ue(&mut self) -> Result<u32, Exhausted> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Exhausted);
            }
        }
        let rest = self.bits(zeros)?;
        let value = (1u64 << zeros) - 1 + rest;
        u32::try_from(value).map_err(|_| Exhausted)
    }

    /// A signed Exp-Golomb code, `se(v)`.
    pub(crate) fn se(&mut self) -> Result<i64, Exhausted> {
        let code = i64::from(self.ue()?);
        Ok(if code % 2 == 1 { (code + 1) / 2 } else { -(code / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_is_removed() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), vec![0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(rbsp(&[0, 3, 0, 0, 3]), vec![0, 3, 0, 0]);
    }

    #[test]
    fn exp_golomb_codes_read_as_written() {
        // 1, 010, 011, 00100, 00101 -> 0, 1, 2, 3, 4.
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&data);
        let read: Vec<u32> = (0..5).map(|_| r.ue().unwrap()).collect();
        assert_eq!(read, vec![0, 1, 2, 3, 4]);
        let mut r = BitReader::new(&data);
        let signed: Vec<i64> = (0..5).map(|_| r.se().unwrap()).collect();
        assert_eq!(signed, vec![0, 1, -1, 2, -2]);
    }

    #[test]
    fn a_read_past_the_end_is_an_error() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.bits(8), Ok(0xff));
        assert_eq!(r.bit(), Err(Exhausted));
        assert_eq!(BitReader::new(&[0; 5]).ue(), Err(Exhausted));
        assert_eq!(BitReader::new(&[0]).skip(9), Err(Exhausted));
        assert_eq!(BitReader::new(&[0; 16]).bits(65), Err(Exhausted));
    }
}
