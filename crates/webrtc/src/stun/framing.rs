//! Where one message ends on a byte stream (TURN over TCP, RFC 8656
//! section 3.1): STUN and ChannelData messages follow one another with no
//! length in front of them. A STUN message is its 20-byte header and the
//! length that header declares; a ChannelData message is its 4-byte header
//! and its length, padded with zeros to a multiple of four. (RFC 4571's
//! two-byte prefix, which ICE-TCP uses on the WebRTC port, is not TURN's.)
//!
//! Strict like the rest of the codec: the first byte must be a STUN message's
//! or a ChannelData channel's, the cookie and the alignment are checked as
//! soon as the bytes are there, and no message is longer than [`MAX_FRAME`],
//! so a server that lies makes the connection fail instead of making the
//! buffer grow.

use super::{HEADER, MAGIC_COOKIE, is_channel_number};

/// The longest message read from a stream. A relayed WebRTC packet is under
/// 1.5 kB; a message beyond this is not one a relay we talk to sends.
pub(crate) const MAX_FRAME: usize = 16 * 1024;

/// Why a stream cannot be read any further: it is not TURN's, or the server
/// is not one we serve. The connection is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BadFrame {
    #[error("a byte that starts neither a STUN message nor a ChannelData message")]
    Start,
    #[error("a STUN message without the magic cookie, or of a length not a multiple of four")]
    Stun,
    #[error("a ChannelData message for a channel outside the range")]
    Channel,
    #[error("a message longer than {MAX_FRAME} bytes")]
    TooLong,
}

/// The length, in bytes, of the message at the start of `buffered`
/// (ChannelData padding included): `None` while more bytes are needed to
/// know it, an error when the stream cannot be TURN's. The message is
/// complete when `buffered` holds that many bytes.
pub(crate) fn frame_len(buffered: &[u8]) -> Result<Option<usize>, BadFrame> {
    let Some(&first) = buffered.first() else { return Ok(None) };
    match first {
        // The two top bits of a STUN message are zero.
        0x00..=0x3F => stun_len(buffered),
        // The channel numbers 0x4000-0x4FFF (RFC 8656 section 12).
        0x40..=0x4F => channel_len(buffered),
        _ => Err(BadFrame::Start),
    }
}

fn stun_len(buffered: &[u8]) -> Result<Option<usize>, BadFrame> {
    if let Some(&[c0, c1, c2, c3]) = buffered.get(4..8).and_then(|c| <&[u8; 4]>::try_from(c).ok())
        && u32::from_be_bytes([c0, c1, c2, c3]) != MAGIC_COOKIE
    {
        return Err(BadFrame::Stun);
    }
    let Some(&[l0, l1]) = buffered.get(2..4).and_then(|l| <&[u8; 2]>::try_from(l).ok()) else { return Ok(None) };
    let length = usize::from(u16::from_be_bytes([l0, l1]));
    if length % 4 != 0 {
        return Err(BadFrame::Stun);
    }
    bounded(HEADER + length)
}

fn channel_len(buffered: &[u8]) -> Result<Option<usize>, BadFrame> {
    let Some(&[c0, c1, l0, l1]) = buffered.get(..4).and_then(|h| <&[u8; 4]>::try_from(h).ok()) else {
        return Ok(None);
    };
    if !is_channel_number(u16::from_be_bytes([c0, c1])) {
        return Err(BadFrame::Channel);
    }
    bounded(4 + usize::from(u16::from_be_bytes([l0, l1])).next_multiple_of(4))
}

fn bounded(total: usize) -> Result<Option<usize>, BadFrame> {
    if total > MAX_FRAME { Err(BadFrame::TooLong) } else { Ok(Some(total)) }
}

/// A stream's bytes cut into messages: what has been read and not yet
/// returned. Never holds more than one message and the bytes read after its
/// start (a read is [`READ`] bytes at most), because [`frame_len`] refuses
/// any message beyond [`MAX_FRAME`] before it is buffered.
#[derive(Debug, Default)]
pub(crate) struct Framer {
    buf: Vec<u8>,
    /// The message [`Framer::ready`] last reported, dropped at the next call.
    ready: usize,
}

impl Framer {
    /// Adds bytes read from the stream.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Whether a whole message is buffered: its length (read with
    /// [`Framer::frame`]) once it is, `None` while more bytes are needed,
    /// an error when the stream is not TURN's, after which the connection
    /// is closed and the framer not used again.
    pub(crate) fn ready(&mut self) -> Result<Option<usize>, BadFrame> {
        self.buf.drain(..self.ready.min(self.buf.len()));
        self.ready = 0;
        match frame_len(&self.buf)? {
            Some(total) if total <= self.buf.len() => {
                self.ready = total;
                Ok(Some(total))
            }
            _ => Ok(None),
        }
    }

    /// The message [`Framer::ready`] reported, until the next call to it.
    pub(crate) fn frame(&self) -> &[u8] {
        self.buf.get(..self.ready).unwrap_or_default()
    }

    /// Bytes held now.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// The most a read adds to a [`Framer`] at once.
pub(crate) const READ: usize = 4096;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::stun::{Class, Encoder, channel_data_padded, method};

    fn binding() -> Vec<u8> {
        Encoder::new(Class::Request, method::BINDING, [7; 12]).finish(None, true)
    }

    #[test]
    fn a_stun_message_is_its_header_and_its_declared_length() {
        let message = binding();
        assert_eq!(message.len(), 28, "a header and a fingerprint");
        assert_eq!(frame_len(&message), Ok(Some(28)));
        // Followed by the next message: only the first one counts.
        let mut two = message.clone();
        two.extend_from_slice(&channel_data_padded(0x4001, b"x").unwrap());
        assert_eq!(frame_len(&two), Ok(Some(28)));
    }

    #[test]
    fn a_channel_data_message_is_padded_to_four() {
        for (size, total) in [(0usize, 4usize), (1, 8), (4, 8), (5, 12), (1200, 1204)] {
            let message = channel_data_padded(0x4000, &vec![9; size]).unwrap();
            assert_eq!(message.len(), total, "{size} bytes of data");
            assert_eq!(frame_len(&message), Ok(Some(total)), "{size} bytes of data");
        }
    }

    #[test]
    fn more_bytes_are_asked_for_until_the_length_is_known() {
        let message = binding();
        assert_eq!(frame_len(&[]), Ok(None));
        for have in 1..4 {
            assert_eq!(frame_len(&message[..have]), Ok(None), "{have} bytes");
        }
        // The length is known from four bytes, whatever follows.
        assert_eq!(frame_len(&message[..4]), Ok(Some(28)));
        let channel = channel_data_padded(0x4002, b"abc").unwrap();
        for have in 1..4 {
            assert_eq!(frame_len(&channel[..have]), Ok(None));
        }
        assert_eq!(frame_len(&channel[..4]), Ok(Some(8)));
    }

    #[test]
    fn what_is_not_turn_is_refused_as_soon_as_it_is_seen() {
        // Not STUN, not a channel: an RTP-looking byte, a DTLS one, zeros past it.
        for first in [0x50u8, 0x80, 0xFF] {
            assert_eq!(frame_len(&[first, 0, 0, 0]), Err(BadFrame::Start));
        }
        // A channel number outside 0x4000-0x4FFF (0x40 is its high byte only).
        assert_eq!(frame_len(&[0x40, 0, 0, 0]), Ok(Some(4)));
        assert_eq!(frame_len(&[0x4F, 0xFF, 0, 1]), Ok(Some(8)));
        // A STUN header with another cookie: seen with the 8th byte.
        let mut wrong = binding();
        wrong[4] ^= 1;
        assert_eq!(frame_len(&wrong), Err(BadFrame::Stun));
        assert_eq!(frame_len(&wrong[..7]), Ok(Some(28)), "the cookie is checked when it is there");
        // A length that is not a multiple of four.
        let mut odd = binding();
        odd[3] = 5;
        assert_eq!(frame_len(&odd), Err(BadFrame::Stun));
    }

    #[test]
    fn a_framer_cuts_a_stream_whatever_the_reads() {
        let mut stream = binding();
        stream.extend_from_slice(&channel_data_padded(0x4001, b"abc").unwrap());
        stream.extend_from_slice(&binding());
        for step in [1, 2, 3, 7, 28, stream.len()] {
            let mut framer = Framer::default();
            let mut lengths = Vec::new();
            for piece in stream.chunks(step) {
                framer.push(piece);
                while let Some(total) = framer.ready().unwrap() {
                    assert_eq!(framer.frame().len(), total);
                    lengths.push(total);
                }
            }
            assert_eq!(lengths, [28, 8, 28], "reads of {step}");
            assert_eq!(framer.buffered(), 0);
        }
    }

    #[test]
    fn a_framer_never_holds_more_than_one_message_and_a_read() {
        // The longest message, read READ bytes at a time: complete only with
        // its last byte; the next message, declared beyond the bound, is
        // refused when its header is read, before anything of it is buffered.
        let mut framer = Framer::default();
        let mut message = vec![0x40, 0x00, 0x3F, 0xFC];
        message.resize(MAX_FRAME, 1);
        let pieces: Vec<&[u8]> = message.chunks(READ).collect();
        for piece in &pieces[..pieces.len() - 1] {
            framer.push(piece);
            assert_eq!(framer.ready(), Ok(None));
        }
        framer.push(pieces[pieces.len() - 1]);
        assert_eq!(framer.ready(), Ok(Some(MAX_FRAME)));
        assert_eq!(framer.buffered(), MAX_FRAME);
        framer.push(&[0x40, 0, 0xFF, 0xFF]);
        assert_eq!(framer.ready(), Err(BadFrame::TooLong));
        assert_eq!(framer.buffered(), 4, "the message before it was dropped, nothing of the bad one kept");
    }

    #[test]
    fn no_message_is_longer_than_the_bound() {
        let mut huge = vec![0x40, 0x00, 0xFF, 0xFF];
        assert_eq!(frame_len(&huge), Err(BadFrame::TooLong));
        huge = binding();
        huge[2..4].copy_from_slice(&(MAX_FRAME as u16).to_be_bytes());
        assert_eq!(frame_len(&huge), Err(BadFrame::TooLong), "the header counts too");
        huge[2..4].copy_from_slice(&((MAX_FRAME - HEADER) as u16).to_be_bytes());
        assert_eq!(frame_len(&huge), Ok(Some(MAX_FRAME)));
    }
}
