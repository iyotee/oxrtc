//! What RTP/JPEG (RFC 2435) needs to know about a JPEG file, read from its
//! markers: the picture's size and chroma layout, the quantization tables,
//! the restart interval, and where the entropy-coded data starts.
//!
//! RFC 2435 sends the scan data and leaves the rest to the receiver, which
//! rebuilds the headers: its limits are therefore this parser's refusals. The
//! picture is baseline (one sequential scan of 8-bit samples), three
//! components, Y at 2x1 or 2x2 and the chroma at 1x1; each side is at most
//! 2040 pixels (the header counts 8-pixel blocks in a byte). The receiver
//! decodes with the standard Huffman tables of ITU-T T.81 Annex K, Y on the
//! luminance ones and the chroma on the chrominance ones: a file coded with
//! tables of its own (an encoder that fits them to the picture) is coded again
//! with the standard ones, without decoding the picture (`recode.rs`).

use std::borrow::Cow;

use thiserror::Error;

use crate::huffman::Table;
use crate::recode::{self, Coding, Refusal, Scan};

/// Largest side RTP/JPEG can announce: its header holds blocks of 8 in a byte.
pub(crate) const MAX_SIDE: u16 = 255 * 8;

/// Why a JPEG cannot be sent as RTP/JPEG.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum JpegError {
    /// Not a JPEG, or cut short before its scan.
    #[error("not a complete JPEG: {0}")]
    Malformed(&'static str),
    /// A valid JPEG that RTP/JPEG cannot carry.
    #[error("not carried by RTP/JPEG: {0}")]
    Unsupported(&'static str),
}

/// A JPEG, as RTP/JPEG describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Jpeg<'a> {
    /// Pixels.
    pub(crate) width: u16,
    /// Pixels.
    pub(crate) height: u16,
    /// RFC 2435 type: 0 for 4:2:2 (Y at 2x1), 1 for 4:2:0 (Y at 2x2), plus 64
    /// when a restart interval is defined.
    pub(crate) kind: u8,
    /// Restart interval in MCUs, when the file defines one.
    pub(crate) restart: Option<u16>,
    /// Luminance table then chrominance table, 64 bytes each, in the zigzag
    /// order of the DQT marker.
    pub(crate) tables: [u8; 128],
    /// The entropy-coded data (restart markers included), without the EOI, as
    /// the standard Huffman tables code it.
    pub(crate) scan: Cow<'a, [u8]>,
}

/// What the markers before the scan said.
#[derive(Default)]
struct Header {
    /// The DQT tables by id (0 to 3), when defined.
    quant: [Option<[u8; 64]>; 4],
    restart: Option<u16>,
    /// The DHT tables by class (DC, then AC) and id (0 to 3), when defined.
    huffman: [[Option<Table>; 4]; 2],
    /// Height, width, then per component its sampling byte and table id.
    frame: Option<Frame>,
}

#[derive(Clone, Copy)]
struct Frame {
    height: u16,
    width: u16,
    /// Per component: its id, its sampling byte and its quantization table.
    components: [(u8, u8, u8); 3],
}

fn malformed<T>(why: &'static str) -> Result<T, JpegError> {
    Err(JpegError::Malformed(why))
}

fn unsupported<T>(why: &'static str) -> Result<T, JpegError> {
    Err(JpegError::Unsupported(why))
}

/// Reads `data` as far as the start of its scan.
pub(crate) fn parse(data: &[u8]) -> Result<Jpeg<'_>, JpegError> {
    let Some(mut rest) = data.strip_prefix(&[0xff, 0xd8]) else { return malformed("no start-of-image marker") };
    let mut header = Header::default();
    loop {
        // A marker is 0xff, any number of 0xff of padding, then its code.
        let Some((&0xff, after)) = rest.split_first() else { return malformed("a marker was expected") };
        let padding = after.iter().take_while(|b| **b == 0xff).count();
        let Some((&code, after)) = after.get(padding..).and_then(<[u8]>::split_first) else {
            return malformed("cut inside a marker");
        };
        rest = after;
        match code {
            // Standalone markers: TEM, RSTn, SOI.
            0x01 | 0xd0..=0xd8 => continue,
            0xd9 => return malformed("end of image before the scan"),
            0x00 => return malformed("a stuffed byte outside a scan"),
            _ => {}
        }
        let Some((length, after)) = rest.split_first_chunk::<2>() else { return malformed("cut inside a length") };
        let body_len = usize::from(u16::from_be_bytes(*length)).saturating_sub(2);
        let Some((body, after)) = after.split_at_checked(body_len) else { return malformed("a segment is cut short") };
        rest = after;
        match code {
            0xdb => quantization(body, &mut header)?,
            0xdd => match body {
                [high, low] => header.restart = Some(u16::from_be_bytes([*high, *low])).filter(|n| *n != 0),
                _ => return malformed("a restart interval of the wrong size"),
            },
            0xc4 => huffman(body, &mut header)?,
            0xc0 | 0xc1 => header.frame = Some(frame(body)?),
            // The other start-of-frame markers: progressive, lossless,
            // arithmetic, hierarchical.
            0xc2 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return unsupported("not a baseline JPEG"),
            0xda => return finish(header, body, rest),
            _ => {}
        }
    }
}

/// DQT: any number of tables, 8-bit only.
fn quantization(mut body: &[u8], header: &mut Header) -> Result<(), JpegError> {
    while let Some((&info, after)) = body.split_first() {
        if info >> 4 != 0 {
            return unsupported("16-bit quantization tables");
        }
        let Some((table, after)) = after.split_first_chunk::<64>() else {
            return malformed("a quantization table is cut short");
        };
        match header.quant.get_mut(usize::from(info & 0x0f)) {
            Some(slot) => *slot = Some(*table),
            None => return malformed("a quantization table id above 3"),
        }
        body = after;
    }
    Ok(())
}

/// DHT: any number of tables, kept for the scan to be read with.
fn huffman(mut body: &[u8], header: &mut Header) -> Result<(), JpegError> {
    while let Some((&info, after)) = body.split_first() {
        let Some((counts, values)) = after.split_first_chunk::<16>() else {
            return malformed("a Huffman table is cut short");
        };
        let count: usize = counts.iter().map(|c| usize::from(*c)).sum();
        let Some((values, after)) = values.split_at_checked(count) else {
            return malformed("a Huffman table is cut short");
        };
        let table = Table::new(counts, values).or_else(malformed)?;
        let slot =
            header.huffman.get_mut(usize::from(info >> 4)).and_then(|class| class.get_mut(usize::from(info & 0x0f)));
        match slot {
            Some(slot) => *slot = Some(table),
            None => return malformed("a Huffman table id out of range"),
        }
        body = after;
    }
    Ok(())
}

/// SOF0 / SOF1.
fn frame(body: &[u8]) -> Result<Frame, JpegError> {
    let Some((&[precision, h1, h0, w1, w0, count], components)) = body.split_first_chunk::<6>() else {
        return malformed("a frame header is cut short");
    };
    if precision != 8 {
        return unsupported("samples that are not 8 bits");
    }
    if count != 3 {
        return unsupported("not three components");
    }
    let Some((&[y_id, y_sampling, y_table, cb_id, cb_sampling, cb_table, cr_id, cr_sampling, cr_table], _)) =
        components.split_first_chunk::<9>()
    else {
        return malformed("a frame header is cut short");
    };
    Ok(Frame {
        height: u16::from_be_bytes([h1, h0]),
        width: u16::from_be_bytes([w1, w0]),
        components: [(y_id, y_sampling, y_table), (cb_id, cb_sampling, cb_table), (cr_id, cr_sampling, cr_table)],
    })
}

/// SOS and what follows: checks the scan and the frame it belongs to.
fn finish<'a>(header: Header, sos: &[u8], data: &'a [u8]) -> Result<Jpeg<'a>, JpegError> {
    let Some(frame) = header.frame else { return malformed("a scan before any frame header") };
    if frame.width == 0 || frame.height == 0 {
        return malformed("a picture without size");
    }
    if frame.width > MAX_SIDE || frame.height > MAX_SIDE {
        return unsupported("a side above 2040 pixels");
    }
    let [(y_id, y, y_table), (cb_id, cb, cb_table), (cr_id, cr, cr_table)] = frame.components;
    let kind = match (y, cb, cr) {
        (0x21, 0x11, 0x11) => 0,
        (0x22, 0x11, 0x11) => 1,
        _ => return unsupported("chroma layout other than 4:2:2 and 4:2:0"),
    };
    if y_table != 0 || cb_table != cr_table || cb_table > 1 {
        return unsupported("quantization tables other than luminance 0 and chrominance 0 or 1");
    }
    // The scan: the three components in the frame's order, the whole spectrum,
    // no successive approximation.
    let Some((&[count, y_scan, y_huffman, cb_scan, cb_huffman, cr_scan, cr_huffman, start, end, approximation], _)) =
        sos.split_first_chunk::<10>()
    else {
        return unsupported("a scan that does not hold the three components");
    };
    if count != 3 || (y_scan, cb_scan, cr_scan) != (y_id, cb_id, cr_id) {
        return unsupported("a scan that does not hold the three components in the frame's order");
    }
    if (start, end, approximation) != (0, 63, 0) {
        return unsupported("a scan that is not baseline");
    }
    let table = |id: usize| header.quant.get(id).copied().flatten();
    let (Some(luma), Some(chroma)) = (table(0), table(usize::from(cb_table))) else {
        return malformed("a quantization table is missing");
    };
    let mut tables = [0u8; 128];
    let (low, high) = tables.split_at_mut(64);
    low.copy_from_slice(&luma);
    high.copy_from_slice(&chroma);
    // The first end of image: padding after it is not the picture's.
    let scan = data.windows(2).position(|pair| pair == [0xff, 0xd9]).map_or(data, |at| data.get(..at).unwrap_or(data));
    if scan.is_empty() {
        return malformed("an empty scan");
    }
    let coded = [(y_huffman, false), (cb_huffman, true), (cr_huffman, true)];
    let scan = standard_coding(&header, coded, (kind, frame.width, frame.height), scan)?;
    Ok(Jpeg {
        width: frame.width,
        height: frame.height,
        kind: kind + if header.restart.is_some() { 64 } else { 0 },
        restart: header.restart,
        tables,
        scan,
    })
}

/// The scan as the standard Huffman tables code it: as it is when the file
/// already uses them (the tables it defines are the standard ones, or none
/// are, and Y is on table 0 and the chroma on table 1), else coded again.
/// `coded` is per component its `Td << 4 | Ta` and whether it is chroma.
fn standard_coding<'a>(
    header: &Header,
    coded: [(u8, bool); 3],
    (kind, width, height): (u8, u16, u16),
    scan: &'a [u8],
) -> Result<Cow<'a, [u8]>, JpegError> {
    let find = |class: usize, id: u8| -> Result<Table, JpegError> {
        let defined = header.huffman.get(class).and_then(|tables| tables.get(usize::from(id))).cloned().flatten();
        match (defined, id) {
            (Some(table), _) => Ok(table),
            // No DHT at all is how a file says "the standard ones" (motion JPEG in AVI).
            (None, 0 | 1) => Ok(Table::standard(class == 1, id == 1)),
            (None, _) => malformed("a scan that uses a Huffman table the file does not define"),
        }
    };
    let mut standard = true;
    let mut coding = Vec::with_capacity(3);
    for (selector, chroma) in coded {
        let (dc, ac) = (find(0, selector >> 4)?, find(1, selector & 0x0f)?);
        standard &= dc.is_standard(false, chroma) && ac.is_standard(true, chroma);
        coding.push(Coding { dc, ac });
    }
    if standard {
        return Ok(Cow::Borrowed(scan));
    }
    let Ok(coding) = <[Coding; 3]>::try_from(coding) else { return malformed("a scan with no three components") };
    let mcu_height = if kind == 0 { 8 } else { 16 };
    let scan = Scan {
        data: scan,
        blocks: [if kind == 0 { 2 } else { 4 }, 1, 1],
        mcus: usize::from(width).div_ceil(16) * usize::from(height).div_ceil(mcu_height),
        restart: header.restart.map(usize::from),
        coding,
    };
    match recode::to_standard(&scan, MAX_SCAN) {
        Ok(recoded) => Ok(Cow::Owned(recoded)),
        Err(Refusal::Malformed(why)) => malformed(why),
        Err(Refusal::TooLarge) => unsupported("a picture too large once coded with the standard tables"),
    }
}

/// The payload header of the first packet, at most: main (8), restart (4),
/// quantization table header (4) and the two tables (128).
const MAX_HEADER: usize = 8 + 4 + 4 + 128;

/// The scan is cut where the 24-bit fragment offset stops.
const MAX_SCAN: usize = 1 << 24;

/// One packet's payload: its RFC 2435 headers, then a piece of the scan.
pub(crate) struct Fragment<'a> {
    header: [u8; MAX_HEADER],
    header_len: usize,
    /// The piece of the scan this packet carries.
    pub(crate) data: &'a [u8],
    /// Whether it is the frame's last packet (the RTP marker bit).
    pub(crate) last: bool,
}

impl Fragment<'_> {
    /// The headers.
    pub(crate) fn header(&self) -> &[u8] {
        self.header.get(..self.header_len).unwrap_or_default()
    }

    fn put(&mut self, bytes: &[u8]) {
        if let Some(slot) = self.header.get_mut(self.header_len..self.header_len + bytes.len()) {
            slot.copy_from_slice(bytes);
            self.header_len += bytes.len();
        }
    }
}

impl Jpeg<'_> {
    /// The packets of the picture, each at most `max_payload` bytes with its
    /// headers. Quantization tables go in the first packet of every frame
    /// (Q = 255: the receiver takes them from there). Nothing for a scan the
    /// 24-bit offset cannot address, or a `max_payload` too small for the
    /// headers.
    pub(crate) fn fragments(&self, max_payload: usize) -> impl Iterator<Item = Fragment<'_>> {
        let jpeg = self;
        let mut offset = 0usize;
        std::iter::from_fn(move || {
            if offset >= jpeg.scan.len() || jpeg.scan.len() >= MAX_SCAN {
                return None;
            }
            let mut out = Fragment { header: [0; MAX_HEADER], header_len: 0, data: &[], last: false };
            let [_, high, mid, low] = u32::try_from(offset).ok()?.to_be_bytes();
            let block = |side: u16| u8::try_from(side.div_ceil(8)).unwrap_or(u8::MAX);
            out.put(&[0, high, mid, low, jpeg.kind, 255, block(jpeg.width), block(jpeg.height)]);
            if let Some(interval) = jpeg.restart {
                // F = 1, L = 1, count 0x3fff: restart intervals are not split.
                let [hi, lo] = interval.to_be_bytes();
                out.put(&[hi, lo, 0xff, 0xff]);
            }
            if offset == 0 {
                out.put(&[0, 0, 0, 128]);
                out.put(&jpeg.tables);
            }
            let room = max_payload.checked_sub(out.header_len).filter(|room| *room > 0)?;
            let rest = jpeg.scan.get(offset..)?;
            let (data, _) = rest.split_at(room.min(rest.len()));
            offset += data.len();
            out.data = data;
            out.last = offset == jpeg.scan.len();
            Some(out)
        })
    }
}

#[cfg(test)]
#[path = "jpeg_tests.rs"]
pub(crate) mod tests;
