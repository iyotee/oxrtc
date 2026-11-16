//! A scan coded with Huffman tables of its own, coded again with the standard
//! ones: the only tables an RTP/JPEG receiver (RFC 2435) knows.
//!
//! The change is lossless and never decodes a picture. Every symbol of a
//! baseline scan is a run length and a size, then that many raw bits of the
//! coefficient; only the code that stands for the symbol changes, the raw bits
//! are copied. Restart markers stay where they are (the stream is cut at them
//! and aligned the way the standard asks), so the result decodes to the same
//! picture as the file, bit for bit.
//!
//! The standard AC tables hold every run and size a baseline 8-bit picture can
//! have and the DC ones every size up to 11, so a valid scan always fits. What
//! does not decode (a code that is none of the table's, a scan cut short, a
//! marker where a symbol belongs, a run past the end of a block) is an error:
//! the picture is dropped, never sent half-converted.

use std::sync::LazyLock;

use crate::huffman::{BitReader, BitWriter, Decoder, Encoder, Table};

/// The tables one component's blocks are coded with: DC then AC.
#[derive(Clone)]
pub(crate) struct Coding {
    pub(crate) dc: Table,
    pub(crate) ac: Table,
}

/// What the scan holds, from the picture's header.
#[derive(Clone)]
pub(crate) struct Scan<'a> {
    /// The entropy-coded data, restart markers included, without the EOI.
    pub(crate) data: &'a [u8],
    /// Blocks of each component in one MCU: luminance 2 (4:2:2) or 4 (4:2:0),
    /// then one each for the two chrominances.
    pub(crate) blocks: [usize; 3],
    /// Minimum coded units in the picture.
    pub(crate) mcus: usize,
    /// MCUs between two restart markers.
    pub(crate) restart: Option<usize>,
    /// The coding of Y, Cb and Cr in the file.
    pub(crate) coding: [Coding; 3],
}

/// Why a scan cannot be coded again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// It does not decode with the tables the file gave.
    Malformed(&'static str),
    /// It decodes, and the result is larger than allowed.
    TooLarge,
}

fn malformed<T>(why: &'static str) -> Result<T, Refusal> {
    Err(Refusal::Malformed(why))
}

/// The standard coder of one component class.
struct Standard {
    dc: Encoder,
    ac: Encoder,
}

impl Standard {
    fn new(chroma: bool) -> Self {
        Self { dc: Encoder::new(&Table::standard(false, chroma)), ac: Encoder::new(&Table::standard(true, chroma)) }
    }
}

static LUMA: LazyLock<Standard> = LazyLock::new(|| Standard::new(false));
static CHROMA: LazyLock<Standard> = LazyLock::new(|| Standard::new(true));

/// The same scan with the standard tables: Y with the luminance ones, Cb and Cr
/// with the chrominance ones. At most `limit` bytes.
pub(crate) fn to_standard(scan: &Scan<'_>, limit: usize) -> Result<Vec<u8>, Refusal> {
    let decoders = scan.coding.each_ref().map(|c| (Decoder::new(&c.dc), Decoder::new(&c.ac)));
    let mut bits = BitReader::new(scan.data);
    let mut out = BitWriter::with_capacity(scan.data.len() + scan.data.len() / 4);
    for mcu in 0..scan.mcus {
        for (component, ((dc, ac), blocks)) in decoders.iter().zip(scan.blocks).enumerate() {
            let encoder: &Standard = if component == 0 { &LUMA } else { &CHROMA };
            for _ in 0..blocks {
                block(&mut bits, &mut out, dc, ac, encoder)?;
            }
        }
        if out.len() > limit {
            return Err(Refusal::TooLarge);
        }
        let done = mcu + 1;
        if done < scan.mcus && scan.restart.is_some_and(|n| n > 0 && done % n == 0) {
            let Some(marker) = bits.restart() else { return malformed("a restart marker was expected") };
            out.align();
            out.marker(marker);
        }
    }
    Ok(out.finish())
}

/// One 8x8 block: its DC symbol and bits, then AC symbols until the end of the
/// block.
fn block(
    bits: &mut BitReader<'_>,
    out: &mut BitWriter,
    dc: &Decoder,
    ac: &Decoder,
    standard: &Standard,
) -> Result<(), Refusal> {
    let size = dc.symbol(bits).ok_or(Refusal::Malformed("a DC code that decodes to nothing"))?;
    if size > 11 || !standard.dc.put(out, size) {
        return malformed("a DC size no baseline picture has");
    }
    copy(bits, out, u32::from(size))?;
    let mut index = 1u32;
    while index < 64 {
        let symbol = ac.symbol(bits).ok_or(Refusal::Malformed("an AC code that decodes to nothing"))?;
        let (run, size) = (u32::from(symbol >> 4), u32::from(symbol & 0x0f));
        if !standard.ac.put(out, symbol) {
            return malformed("an AC run and size no baseline picture has");
        }
        match (run, size) {
            // End of block.
            (0, 0) => return Ok(()),
            // Sixteen zeros.
            (15, 0) => index += 16,
            (_, 0) => return malformed("an AC run with no size"),
            _ => {
                index += run + 1;
                copy(bits, out, size)?;
            }
        }
    }
    if index > 64 { malformed("a run past the end of a block") } else { Ok(()) }
}

/// `count` raw bits, as they are.
fn copy(bits: &mut BitReader<'_>, out: &mut BitWriter, count: u32) -> Result<(), Refusal> {
    let value = bits.bits(count).ok_or(Refusal::Malformed("a scan that ends inside a coefficient"))?;
    out.put(value, count);
    Ok(())
}

#[cfg(test)]
#[path = "recode_tests.rs"]
mod tests;
