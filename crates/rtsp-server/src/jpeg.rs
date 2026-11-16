//! What RTP/JPEG (RFC 2435) needs to know about a JPEG file, read from its
//! markers: the picture's size and chroma layout, the quantization tables,
//! the restart interval, and where the entropy-coded data starts.
//!
//! RFC 2435 sends the scan data and leaves the rest to the receiver, which
//! rebuilds the headers: its limits are therefore this parser's refusals. The
//! picture is baseline (one sequential scan of 8-bit samples), three
//! components, Y at 2x1 or 2x2 and the chroma at 1x1; the Huffman tables are
//! the standard ones of ITU-T T.81 Annex K (a file that carries other tables
//! would decode into noise on the far side, so it is refused instead); each
//! side is at most 2040 pixels (the header counts 8-pixel blocks in a byte).

use thiserror::Error;

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
    /// The entropy-coded data (restart markers included), without the EOI.
    pub(crate) scan: &'a [u8],
}

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

/// What the markers before the scan said.
#[derive(Default)]
struct Header {
    /// The DQT tables by id (0 to 3), when defined.
    quant: [Option<[u8; 64]>; 4],
    restart: Option<u16>,
    /// Height, width, then per component its sampling byte and table id.
    frame: Option<Frame>,
}

struct Frame {
    height: u16,
    width: u16,
    components: [(u8, u8); 3],
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
            0xc4 => huffman(body)?,
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

/// DHT: the tables must be the standard ones, which the receiver assumes.
fn huffman(mut body: &[u8]) -> Result<(), JpegError> {
    while let Some((&info, after)) = body.split_first() {
        let Some((counts, values)) = after.split_first_chunk::<16>() else {
            return malformed("a Huffman table is cut short");
        };
        let count: usize = counts.iter().map(|c| usize::from(*c)).sum();
        let Some((values, after)) = values.split_at_checked(count) else {
            return malformed("a Huffman table is cut short");
        };
        let standard: &[u8] = match info {
            0x00 => &DC_LUMA,
            0x01 => &DC_CHROMA,
            0x10 => &AC_LUMA,
            0x11 => &AC_CHROMA,
            _ => return unsupported("Huffman table other than the four standard ones"),
        };
        let (std_counts, std_values) = standard.split_at(16);
        if std_counts != counts || std_values != values {
            return unsupported("custom Huffman tables");
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
    let Some((&[_, y_sampling, y_table, _, cb_sampling, cb_table, _, cr_sampling, cr_table], _)) =
        components.split_first_chunk::<9>()
    else {
        return malformed("a frame header is cut short");
    };
    Ok(Frame {
        height: u16::from_be_bytes([h1, h0]),
        width: u16::from_be_bytes([w1, w0]),
        components: [(y_sampling, y_table), (cb_sampling, cb_table), (cr_sampling, cr_table)],
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
    let [(y, y_table), (cb, cb_table), (cr, cr_table)] = frame.components;
    let kind = match (y, cb, cr) {
        (0x21, 0x11, 0x11) => 0,
        (0x22, 0x11, 0x11) => 1,
        _ => return unsupported("chroma layout other than 4:2:2 and 4:2:0"),
    };
    if y_table != 0 || cb_table != cr_table || cb_table > 1 {
        return unsupported("quantization tables other than luminance 0 and chrominance 0 or 1");
    }
    // The scan: 3 components, Y on Huffman tables 0 and the chroma on 1, the
    // whole spectrum, no successive approximation.
    let Some((&[count, _, y_huffman, _, cb_huffman, _, cr_huffman, spectral_start, spectral_end, approximation], _)) =
        sos.split_first_chunk::<10>()
    else {
        return unsupported("a scan that does not hold the three components");
    };
    if count != 3 || (y_huffman, cb_huffman, cr_huffman) != (0x00, 0x11, 0x11) {
        return unsupported("a scan with other Huffman table ids than the standard ones");
    }
    if (spectral_start, spectral_end, approximation) != (0, 63, 0) {
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
    let scan = data.strip_suffix(&[0xff, 0xd9]).unwrap_or(data);
    if scan.is_empty() {
        return malformed("an empty scan");
    }
    Ok(Jpeg {
        width: frame.width,
        height: frame.height,
        kind: kind + if header.restart.is_some() { 64 } else { 0 },
        restart: header.restart,
        tables,
        scan,
    })
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

impl<'a> Jpeg<'a> {
    /// The packets of the picture, each at most `max_payload` bytes with its
    /// headers. Quantization tables go in the first packet of every frame
    /// (Q = 255: the receiver takes them from there). Nothing for a scan the
    /// 24-bit offset cannot address, or a `max_payload` too small for the
    /// headers.
    pub(crate) fn fragments(&self, max_payload: usize) -> impl Iterator<Item = Fragment<'a>> + use<'a> {
        let jpeg = self.clone();
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
mod tests;
