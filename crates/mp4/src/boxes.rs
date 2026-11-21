//! Writing boxes (ISO/IEC 14496-12): a size, a type, a body. A box is opened,
//! written into, and closed, which writes its size; a size over 32 bits is an
//! error, never a wrapped number.

use crate::Mp4Error;

/// A byte buffer of nested boxes.
#[derive(Debug, Default)]
pub(crate) struct BoxWriter {
    buf: Vec<u8>,
}

/// An open box: where its size goes.
#[must_use = "a box is closed with BoxWriter::end"]
pub(crate) struct Open(usize);

impl BoxWriter {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { buf: Vec::with_capacity(capacity) }
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// Opens a box of type `kind`.
    pub(crate) fn start(&mut self, kind: &[u8; 4]) -> Open {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        self.buf.extend_from_slice(kind);
        Open(at)
    }

    /// Opens a full box: a version and 24 bits of flags after the type.
    pub(crate) fn start_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> Open {
        let open = self.start(kind);
        self.u32((u32::from(version) << 24) | (flags & 0x00ff_ffff));
        open
    }

    /// Closes a box: writes its size.
    pub(crate) fn end(&mut self, open: Open) -> Result<(), Mp4Error> {
        let size = self.buf.len().checked_sub(open.0).ok_or(Mp4Error::invalid("box", "closed before it opened"))?;
        let size = u32::try_from(size).map_err(|_| Mp4Error::invalid("box", "over 4 GiB"))?;
        self.patch_u32(open.0, size)
    }

    /// Writes `value` at `at`, in place of four bytes already written.
    pub(crate) fn patch_u32(&mut self, at: usize, value: u32) -> Result<(), Mp4Error> {
        let field = at
            .checked_add(4)
            .and_then(|end| self.buf.get_mut(at..end))
            .ok_or(Mp4Error::invalid("box", "a field patched past the end"))?;
        field.copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u24(&mut self, value: u32) {
        self.buf.extend_from_slice(value.to_be_bytes().get(1..).unwrap_or_default());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub(crate) fn zeros(&mut self, count: usize) {
        self.buf.resize(self.buf.len() + count, 0);
    }

    /// The unity transformation matrix of `mvhd` and `tkhd`.
    pub(crate) fn matrix(&mut self) {
        for value in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(value);
        }
    }
}

/// A `VisualSampleEntry` of type `kind` around its configuration box.
pub(crate) fn visual_entry(kind: &[u8; 4], width: u16, height: u16, config: &[u8]) -> Result<Vec<u8>, Mp4Error> {
    let mut w = BoxWriter::with_capacity(86 + config.len());
    let entry = w.start(kind);
    w.zeros(6);
    w.u16(1); // data_reference_index
    w.zeros(16); // pre_defined, reserved
    w.u16(width);
    w.u16(height);
    w.u32(0x0048_0000); // 72 dpi
    w.u32(0x0048_0000);
    w.u32(0);
    w.u16(1); // frame_count
    w.zeros(32); // compressorname
    w.u16(0x0018); // depth
    w.u16(0xffff); // pre_defined = -1
    w.bytes(config);
    w.end(entry)?;
    Ok(w.into_inner())
}

/// An `AudioSampleEntry` of type `kind` around its configuration box. A rate
/// over 16 bits is written as 0, as the field cannot hold it (the
/// configuration box carries the real one).
pub(crate) fn audio_entry(kind: &[u8; 4], channels: u16, rate: u32, config: &[u8]) -> Result<Vec<u8>, Mp4Error> {
    let mut w = BoxWriter::with_capacity(36 + config.len());
    let entry = w.start(kind);
    w.zeros(6);
    w.u16(1); // data_reference_index
    w.zeros(8); // reserved
    w.u16(channels);
    w.u16(16); // samplesize
    w.u16(0); // pre_defined
    w.u16(0); // reserved
    w.u32(u16::try_from(rate).map_or(0, |rate| u32::from(rate) << 16));
    w.bytes(config);
    w.end(entry)?;
    Ok(w.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_carries_its_size_and_type() {
        let mut w = BoxWriter::default();
        let outer = w.start(b"moov");
        let inner = w.start_full(b"mvhd", 1, 0x0102_0304);
        w.u24(0x0a0b0c);
        w.end(inner).unwrap();
        w.end(outer).unwrap();
        let out = w.into_inner();
        assert_eq!(&out[..8], &[0, 0, 0, 23, b'm', b'o', b'o', b'v']);
        assert_eq!(&out[8..16], &[0, 0, 0, 15, b'm', b'v', b'h', b'd']);
        assert_eq!(&out[16..], &[1, 2, 3, 4, 0x0a, 0x0b, 0x0c]);
    }

    #[test]
    fn a_rate_over_16_bits_is_written_as_zero() {
        let entry = audio_entry(b"fLaC", 1, 96_000, &[]).unwrap();
        assert_eq!(&entry[32..36], &[0; 4]);
        let entry = audio_entry(b"fLaC", 1, 48_000, &[]).unwrap();
        assert_eq!(&entry[32..36], &[0xbb, 0x80, 0, 0]);
    }

    #[test]
    fn a_patch_past_the_end_is_an_error() {
        let mut w = BoxWriter::default();
        w.u16(0);
        assert!(w.patch_u32(0, 1).is_err());
    }
}
