//! What every egress of Motion JPEG needs to know about a JPEG file, read
//! from its markers (ITU-T T.81 annex B): where a picture ends in a byte
//! stream of pictures one after the other (FFmpeg's `-f mjpeg` output, M5
//! F5), its size, and whether it carries its Huffman tables.
//!
//! A Motion JPEG picture may leave its Huffman tables out and count on the
//! standard ones of annex K.3 (an `AVI1` picture, a webcam's). Today's
//! browsers assume them (measured 2026-11-24: Firefox 159 and Edge draw such
//! a picture in an `<img>`; Mozilla bug 963907 was Firefox's, since fixed),
//! but not every decoder a still is handed to does (go2rtc's `FixJPEG` names
//! Rust's jpeg-decoder and pion's mediadevices), so [`with_huffman_tables`]
//! writes them in. The same
//! tables are what an RTP/JPEG receiver assumes (`oxrtc-rtsp-server`'s
//! `huffman.rs` reads them from here).
//!
//! Everything is bounded by the input: a reader that runs out of bytes waits
//! for more, a stream that does not hold a picture within its limit is an
//! error, never memory.

use bytes::{Buf, Bytes, BytesMut};

/// The tables of ITU-T T.81 annex K.3, as a DHT body carries them: 16 counts
/// then the values. The luminance's DC coefficients.
pub const DC_LUMA: [u8; 28] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// The chrominance's DC coefficients.
pub const DC_CHROMA: [u8; 28] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// The luminance's AC coefficients.
pub const AC_LUMA: [u8; 178] = [
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
/// The chrominance's AC coefficients.
pub const AC_CHROMA: [u8; 178] = [
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

/// Start of image.
const SOI: u8 = 0xd8;
/// End of image.
const EOI: u8 = 0xd9;
/// Start of scan.
const SOS: u8 = 0xda;
/// Define Huffman tables.
const DHT: u8 = 0xc4;
/// Define quantization tables.
const DQT: u8 = 0xdb;

/// The four standard tables as one DHT segment, marker included: the
/// luminance's DC (class 0, id 0) and AC (1, 0), the chrominance's DC (0, 1)
/// and AC (1, 1), as an `AVI1` picture's decoder assumes them.
#[must_use]
pub fn standard_dht() -> Vec<u8> {
    let tables: [(u8, &[u8]); 4] = [(0x00, &DC_LUMA), (0x10, &AC_LUMA), (0x01, &DC_CHROMA), (0x11, &AC_CHROMA)];
    let body: usize = tables.iter().map(|(_, t)| 1 + t.len()).sum();
    let length = u16::try_from(body + 2).unwrap_or(u16::MAX);
    let mut out = Vec::with_capacity(body + 4);
    out.extend_from_slice(&[0xff, DHT]);
    out.extend_from_slice(&length.to_be_bytes());
    for (class_id, table) in tables {
        out.push(class_id);
        out.extend_from_slice(table);
    }
    out
}

/// What the header segments of a picture say, read up to its first scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// Where the first scan's SOS marker is, when the header reaches it.
    pub first_scan: Option<usize>,
    /// Whether a DHT came before it.
    pub has_tables: bool,
    /// Width and height from the frame header (SOF), when there is one.
    pub size: Option<(u16, u16)>,
}

/// Reads the segments of `jpeg` from its SOI to its first SOS. `None` for
/// what does not start as a JPEG or breaks before its scan.
#[must_use]
pub fn header(jpeg: &[u8]) -> Option<Header> {
    if jpeg.get(..2) != Some(&[0xff, SOI]) {
        return None;
    }
    let mut at = 2;
    let mut read = Header::default();
    loop {
        // Fill bytes (0xFF) may stand before any marker (T.81 B.1.1.2).
        while jpeg.get(at) == Some(&0xff) && jpeg.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        if jpeg.get(at) != Some(&0xff) {
            return None;
        }
        let marker = *jpeg.get(at + 1)?;
        if marker == SOS {
            read.first_scan = Some(at);
            return Some(read);
        }
        if marker == EOI || marker == SOI {
            return None;
        }
        if standalone(marker) {
            at += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([*jpeg.get(at + 2)?, *jpeg.get(at + 3)?]));
        if length < 2 {
            return None;
        }
        if marker == DHT {
            read.has_tables = true;
        }
        // SOF0 to SOF15, but DHT (C4), JPG (C8) and DAC (CC).
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = u16::from_be_bytes([*jpeg.get(at + 5)?, *jpeg.get(at + 6)?]);
            let width = u16::from_be_bytes([*jpeg.get(at + 7)?, *jpeg.get(at + 8)?]);
            read.size = Some((width, height));
        }
        at = at.checked_add(2 + length)?;
    }
}

/// A marker with no length after it: RST0 to RST7 and TEM.
fn standalone(marker: u8) -> bool {
    (0xd0..=0xd7).contains(&marker) || marker == 0x01
}

/// `jpeg` with the standard Huffman tables written before its first scan
/// when its header defines none (an `AVI1` picture, a webcam's); the same
/// bytes otherwise, and for what is not a readable JPEG (it is passed on as
/// it came, as every picture is).
#[must_use]
pub fn with_huffman_tables(jpeg: &Bytes) -> Bytes {
    let Some(Header { first_scan: Some(at), has_tables: false, .. }) = header(jpeg) else {
        return jpeg.clone();
    };
    let dht = standard_dht();
    let mut out = BytesMut::with_capacity(jpeg.len() + dht.len());
    out.extend_from_slice(jpeg.get(..at).unwrap_or_default());
    out.extend_from_slice(&dht);
    out.extend_from_slice(jpeg.get(at..).unwrap_or_default());
    out.freeze()
}

/// A turn of a still, clockwise (go2rtc's `rotate` of `frame.jpeg`: `90`,
/// `180`, `270` or `-90`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Rotate {
    /// As it is.
    #[default]
    None,
    /// A quarter turn clockwise (`90`).
    Quarter,
    /// A half turn (`180`).
    Half,
    /// A quarter turn counterclockwise (`270`, `-90`).
    ThreeQuarters,
}

/// What a still is asked to look like (go2rtc's `width`/`w`, `height`/`h`
/// and `rotate`): scaled to the width or the height given, the other side
/// keeping the picture's shape, or to both, then turned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Shape {
    /// Pixels wide, before the turn.
    pub width: Option<u32>,
    /// Pixels high, before the turn.
    pub height: Option<u32>,
    /// The turn.
    pub rotate: Rotate,
}

impl Shape {
    /// Whether the picture is asked as it is.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        *self == Self::default()
    }
}

/// Why a byte stream of pictures cannot be read on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    /// A picture longer than the limit, or that long without one.
    #[error("no picture within {0} bytes")]
    TooLarge(usize),
}

/// Where the reader of one picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Looking for a start of image; bytes before it are dropped.
    Start,
    /// At a marker of the picture's header (or between two scans).
    Marker(usize),
    /// Inside entropy-coded data, at this byte.
    Scan(usize),
}

/// Cuts a byte stream of JPEG pictures, one after the other with nothing
/// between them (FFmpeg's `-f mjpeg`), into its pictures, as the bytes come.
/// Each is cut at its end of image, found by walking its segments and
/// skipping the entropy-coded data (where a 0xFF byte is always followed by
/// 0x00 or a restart marker), never by searching for 0xFF 0xD9 (an Exif
/// thumbnail holds one). Bytes that do not start a picture are dropped, and a
/// picture broken before its end is dropped from where it breaks.
#[derive(Debug)]
pub struct Splitter {
    buf: BytesMut,
    at: At,
    max: usize,
}

impl Splitter {
    /// A splitter that holds at most `max` bytes of one picture.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self { buf: BytesMut::new(), at: At::Start, max: max.max(4) }
    }

    /// Adds bytes read.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Bytes held: at most the limit and the last read.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next whole picture, `None` until one is held. An error when the
    /// limit is passed without one; the bytes held are then dropped, and the
    /// splitter starts again at the next picture.
    pub fn next_picture(&mut self) -> Result<Option<Bytes>, SplitError> {
        loop {
            match self.at {
                At::Start => {
                    let Some(start) = self.buf.windows(2).position(|w| w == [0xff, SOI]) else {
                        // Keep a last 0xFF: it may be the first half of a SOI.
                        let keep = usize::from(self.buf.last() == Some(&0xff));
                        self.buf.advance(self.buf.len() - keep);
                        return Ok(None);
                    };
                    self.buf.advance(start);
                    self.at = At::Marker(2);
                }
                At::Marker(at) => {
                    let (Some(&first), Some(&marker)) = (self.buf.get(at), self.buf.get(at + 1)) else {
                        return self.wait();
                    };
                    if first != 0xff {
                        self.broken(at);
                        continue;
                    }
                    match marker {
                        0xff => self.at = At::Marker(at + 1),
                        EOI => {
                            let picture = self.buf.split_to(at + 2).freeze();
                            self.at = At::Start;
                            // Read whole in one go, it can still be too long.
                            if picture.len() > self.max {
                                return Err(SplitError::TooLarge(self.max));
                            }
                            return Ok(Some(picture));
                        }
                        SOI => self.broken(at),
                        m if standalone(m) => self.at = At::Marker(at + 2),
                        m => {
                            let (Some(&high), Some(&low)) = (self.buf.get(at + 2), self.buf.get(at + 3)) else {
                                return self.wait();
                            };
                            let length = usize::from(u16::from_be_bytes([high, low]));
                            if length < 2 {
                                self.broken(at);
                                continue;
                            }
                            let next = at + 2 + length;
                            self.at = if m == SOS { At::Scan(next) } else { At::Marker(next) };
                        }
                    }
                }
                At::Scan(from) => {
                    let found = self.buf.get(from..).and_then(|rest| {
                        rest.windows(2).position(|w| {
                            w.first() == Some(&0xff) && w.get(1).is_some_and(|&b| b != 0 && !standalone(b))
                        })
                    });
                    match found {
                        Some(offset) => self.at = At::Marker(from + offset),
                        None => {
                            // Resume where the search stopped, but for a
                            // last 0xFF whose next byte is still to come.
                            let held = self.buf.len().max(from);
                            self.at = At::Scan(held.saturating_sub(1).max(from));
                            return self.wait();
                        }
                    }
                }
            }
        }
    }

    /// Nothing more until more bytes come, unless the limit is passed.
    fn wait(&mut self) -> Result<Option<Bytes>, SplitError> {
        if self.buf.len() > self.max {
            self.buf.clear();
            self.at = At::Start;
            return Err(SplitError::TooLarge(self.max));
        }
        Ok(None)
    }

    /// The picture held breaks at `at`: it is dropped up to there, and the
    /// next start of image is looked for from there.
    fn broken(&mut self, at: usize) {
        self.buf.advance(at.min(self.buf.len()));
        self.at = At::Start;
    }
}

/// Whether a still is better encoded again before it is served as a file,
/// as go2rtc's `FixJPEG` judges a Motion JPEG picture for `frame.jpeg`: one
/// whose first segment after its start of image is neither a quantization
/// table nor a JFIF, Exif or `AVI1` segment (an mjpg-streamer camera's, a
/// comment first) is not recognised by the tools that sniff a file's type
/// from those bytes (Python's `imghdr`) and is refused by image services
/// such as Telegram's; encoded again, it carries a JFIF header. Too short a
/// picture, or what is not one, is left as it is.
#[must_use]
pub fn wants_encoding_again(jpeg: &[u8]) -> bool {
    if jpeg.len() < 10 || jpeg.get(..2) != Some(&[0xff, SOI]) || jpeg.get(2..4) == Some(&[0xff, DQT]) {
        return false;
    }
    !matches!(jpeg.get(6..10), Some(b"JFIF" | b"Exif" | b"AVI1"))
}

#[path = "jpeg_invariants.rs"]
mod invariants;
#[doc(hidden)]
pub use invariants::{split_all, split_invariants, tables_invariants};

#[cfg(test)]
#[path = "jpeg_tests.rs"]
mod tests;
