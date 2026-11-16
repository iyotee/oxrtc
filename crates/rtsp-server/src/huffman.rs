//! Huffman tables as a JPEG file defines them (DHT), the standard ones of
//! ITU-T T.81 Annex K that an RTP/JPEG receiver assumes, and the bit reader
//! and writer that change a scan from one to the other (`recode.rs`).
//!
//! Everything here is bounded by the input: a reader that runs out of bytes or
//! meets a marker stops, a table that is not a prefix code is refused.

/// The tables of ITU-T T.81 Annex K.3, as a DHT body carries them: 16 counts
/// then the values.
const DC_LUMA: [u8; 28] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA: [u8; 28] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA: [u8; 178] = [
    0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d, //
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, //
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, //
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, //
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, //
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, //
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, //
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, //
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, //
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, //
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, //
    0xf9, 0xfa,
];
const AC_CHROMA: [u8; 178] = [
    0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77, //
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, //
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, //
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, //
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, //
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, //
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, //
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, //
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, //
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, //
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, //
    0xf9, 0xfa,
];

fn standard_body(ac: bool, chroma: bool) -> &'static [u8] {
    match (ac, chroma) {
        (false, false) => &DC_LUMA,
        (false, true) => &DC_CHROMA,
        (true, false) => &AC_LUMA,
        (true, true) => &AC_CHROMA,
    }
}

/// A table as a DHT defines it: how many codes there are of each length from 1
/// to 16 bits, then the symbols they stand for, shortest code first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    counts: [u8; 16],
    values: Vec<u8>,
}

impl Table {
    /// The table a DHT body holds: 16 counts then as many values as they add
    /// up to. `Err` for a body of another size, and for counts that are not a
    /// prefix code (more codes of a length than the length has).
    pub(crate) fn new(counts: &[u8; 16], values: &[u8]) -> Result<Self, &'static str> {
        let total: usize = counts.iter().map(|c| usize::from(*c)).sum();
        if total != values.len() || total > 256 {
            return Err("a Huffman table of the wrong size");
        }
        let table = Self { counts: *counts, values: values.to_vec() };
        table.first_codes()?;
        Ok(table)
    }

    /// The standard table (T.81 Annex K.3) of a DC or AC coefficient, for the
    /// luminance or the chrominance.
    pub(crate) fn standard(ac: bool, chroma: bool) -> Self {
        let (counts, values) = standard_body(ac, chroma).split_at(16);
        Self { counts: counts.try_into().unwrap_or([0; 16]), values: values.to_vec() }
    }

    /// Whether this is the standard table of `standard`'s arguments.
    pub(crate) fn is_standard(&self, ac: bool, chroma: bool) -> bool {
        let (counts, values) = standard_body(ac, chroma).split_at(16);
        self.counts == counts && self.values == values
    }

    /// The body of a DHT segment (without its class and id byte).
    #[cfg(test)]
    pub(crate) fn dht_body(&self) -> Vec<u8> {
        [self.counts.as_slice(), &self.values].concat()
    }

    /// The first code of each length, as canonical coding assigns them: the
    /// codes of length `n` are `first[n - 1]..first[n - 1] + counts[n - 1]`.
    fn first_codes(&self) -> Result<[u32; 16], &'static str> {
        let mut first = [0u32; 16];
        let mut code = 0u32;
        for (length, (slot, count)) in first.iter_mut().zip(&self.counts).enumerate() {
            *slot = code;
            code += u32::from(*count);
            if code > 1 << (length + 1) {
                return Err("a Huffman table that is not a prefix code");
            }
            code <<= 1;
        }
        Ok(first)
    }
}

/// Reads symbols of a [`Table`], bit by bit.
pub(crate) struct Decoder {
    first: [u32; 16],
    counts: [u8; 16],
    /// Index into `values` of the first symbol of each length.
    offset: [usize; 16],
    values: Vec<u8>,
}

impl Decoder {
    /// A decoder of `table`.
    pub(crate) fn new(table: &Table) -> Self {
        let mut offset = [0usize; 16];
        let mut at = 0usize;
        for (slot, count) in offset.iter_mut().zip(&table.counts) {
            *slot = at;
            at += usize::from(*count);
        }
        // Valid by construction of a `Table`.
        let first = table.first_codes().unwrap_or([0; 16]);
        Self { first, counts: table.counts, offset, values: table.values.clone() }
    }

    /// The next symbol of `bits`, or `None` when the stream ends first or the
    /// bits are no code of the table.
    pub(crate) fn symbol(&self, bits: &mut BitReader<'_>) -> Option<u8> {
        let mut code = 0u32;
        for length in 0..16 {
            code = code << 1 | bits.bit()?;
            let count = u32::from(*self.counts.get(length)?);
            let first = *self.first.get(length)?;
            if count > 0 && code >= first && code - first < count {
                let index = *self.offset.get(length)? + usize::try_from(code - first).ok()?;
                return self.values.get(index).copied();
            }
        }
        None
    }
}

/// Writes symbols with a [`Table`]: the code and the length of each.
pub(crate) struct Encoder {
    code: [u16; 256],
    length: [u8; 256],
}

impl Encoder {
    /// An encoder with `table`'s codes.
    pub(crate) fn new(table: &Table) -> Self {
        let mut encoder = Self { code: [0; 256], length: [0; 256] };
        let first = table.first_codes().unwrap_or([0; 16]);
        let mut values = table.values.iter();
        for (index, (count, first)) in table.counts.iter().zip(first).enumerate() {
            for n in 0..u32::from(*count) {
                let Some(value) = values.next() else { return encoder };
                let slot = usize::from(*value);
                if let (Some(code), Some(length)) = (encoder.code.get_mut(slot), encoder.length.get_mut(slot)) {
                    *code = u16::try_from(first + n).unwrap_or(0);
                    *length = u8::try_from(index + 1).unwrap_or(0);
                }
            }
        }
        encoder
    }

    /// Writes `symbol`. `false` when the table has no code for it.
    pub(crate) fn put(&self, bits: &mut BitWriter, symbol: u8) -> bool {
        let slot = usize::from(symbol);
        match (self.code.get(slot), self.length.get(slot)) {
            (Some(code), Some(length)) if *length > 0 => {
                bits.put(u32::from(*code), u32::from(*length));
                true
            }
            _ => false,
        }
    }
}

/// Why a [`BitReader`] has no more bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The data ended.
    End,
    /// A marker (its code) was met: a restart, or the end of the scan.
    Marker(u8),
}

/// The bits of entropy-coded data, with its byte stuffing (`0xff 0x00`) undone.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u32,
    left: u32,
    stopped: Option<Stop>,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, acc: 0, left: 0, stopped: None }
    }

    fn refill(&mut self) -> Result<(), Stop> {
        if let Some(stop) = self.stopped {
            return Err(stop);
        }
        let Some(&byte) = self.data.get(self.pos) else { return self.stop(Stop::End) };
        self.pos += 1;
        self.acc = u32::from(byte);
        self.left = 8;
        if byte != 0xff {
            return Ok(());
        }
        // 0xff is a data byte when 0x00 follows it (stuffing), else the start
        // of a marker; 0xff 0xff is fill before one.
        let mut next = self.data.get(self.pos).copied();
        while next == Some(0xff) {
            self.pos += 1;
            next = self.data.get(self.pos).copied();
        }
        match next {
            Some(0x00) => {
                self.pos += 1;
                Ok(())
            }
            Some(code) => {
                self.pos += 1;
                self.stop(Stop::Marker(code))
            }
            None => self.stop(Stop::End),
        }
    }

    fn stop<T>(&mut self, stop: Stop) -> Result<T, Stop> {
        self.acc = 0;
        self.left = 0;
        self.stopped = Some(stop);
        Err(stop)
    }

    /// The next bit.
    pub(crate) fn bit(&mut self) -> Option<u32> {
        if self.left == 0 {
            self.refill().ok()?;
        }
        self.left -= 1;
        Some((self.acc >> self.left) & 1)
    }

    /// The next `n` bits (at most 16), the first one the highest.
    pub(crate) fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |value, _| Some(value << 1 | self.bit()?))
    }

    /// At the end of a restart interval: drops the bits left of the byte and
    /// takes the restart marker that follows, `Some(its code)` for `0xd0` to
    /// `0xd7`. Reading goes on after it.
    pub(crate) fn restart(&mut self) -> Option<u8> {
        self.left = 0;
        self.acc = 0;
        let code = match self.stopped.take() {
            Some(Stop::Marker(code)) => code,
            Some(Stop::End) => return None,
            None => {
                let mut at = self.pos;
                while self.data.get(at) == Some(&0xff) && self.data.get(at + 1) == Some(&0xff) {
                    at += 1;
                }
                if self.data.get(at) != Some(&0xff) {
                    return None;
                }
                self.pos = at + 2;
                *self.data.get(at + 1)?
            }
        };
        (0xd0..=0xd7).contains(&code).then_some(code)
    }
}

/// Entropy-coded data being written, with byte stuffing.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    held: u32,
}

impl BitWriter {
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self { out: Vec::with_capacity(bytes), acc: 0, held: 0 }
    }

    /// The bytes written so far (whole bytes only).
    pub(crate) fn len(&self) -> usize {
        self.out.len()
    }

    /// Writes the `length` low bits of `value` (at most 16).
    pub(crate) fn put(&mut self, value: u32, length: u32) {
        self.acc = (self.acc << length) | (value & ((1u32 << length) - 1));
        self.held += length;
        while self.held >= 8 {
            self.held -= 8;
            let byte = u8::try_from((self.acc >> self.held) & 0xff).unwrap_or(0);
            self.out.push(byte);
            if byte == 0xff {
                self.out.push(0);
            }
        }
        self.acc &= (1u32 << self.held) - 1;
    }

    /// Ends the byte in progress with ones, as the standard asks before a marker.
    pub(crate) fn align(&mut self) {
        if self.held > 0 {
            self.put((1u32 << (8 - self.held)) - 1, 8 - self.held);
        }
    }

    /// A marker, not stuffed.
    pub(crate) fn marker(&mut self, code: u8) {
        self.out.extend_from_slice(&[0xff, code]);
    }

    /// The finished data: the last byte completed with ones.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}
