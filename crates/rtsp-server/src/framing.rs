//! Splitting the bytes of an RTSP connection into messages: requests (parsed
//! by `rtsp-types`) and RTP/RTCP interleaved in `$`-prefixed frames.
//!
//! The split is incremental: each byte read is examined once while the
//! framer looks for the end of a message's first line and of its headers, and
//! `rtsp-types` parses a message at most three times (first line complete,
//! headers complete, body complete) - never once per read. A sender that
//! trickles a request one byte at a time costs work in proportion to what it
//! sends, not to its square.

use bytes::{Buf, Bytes, BytesMut};
use rtsp_types::{Message, ParseError, Request};

/// An interleaved frame: `$`, channel, 16-bit length, then the packet.
const INTERLEAVED_HEADER: usize = 4;

/// Largest interleaved frame: the length field is 16 bits.
const MAX_INTERLEAVED: usize = INTERLEAVED_HEADER + u16::MAX as usize;

/// One message from the peer.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// A request.
    Request(Request<Vec<u8>>),
    /// An RTP or RTCP packet on an interleaved channel.
    Data {
        /// The channel.
        channel: u8,
        /// The packet.
        payload: Bytes,
    },
    /// Something a server ignores (a response to nothing it asked).
    Ignored,
}

/// Why the connection cannot go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    /// A request larger than the limit.
    TooLarge,
    /// Bytes that are not RTSP.
    Malformed,
}

/// Where the framer is in the message at the front of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Between messages.
    Start,
    /// In the first line or the headers of a request (or response).
    Head {
        /// Bytes of the message examined so far.
        scanned: usize,
        /// How much of `\r\n\r\n` the last bytes examined match (0 to 3).
        matched: u8,
        /// Whether the first line has ended.
        line_done: bool,
    },
    /// Headers complete; the body is still arriving.
    Body {
        /// Length of the message's head, headers included.
        head: usize,
        /// Buffer length at which the message is worth parsing again.
        needed: usize,
    },
}

/// A parse attempt's outcome, the buffer left as it was.
enum Attempt {
    /// A message of `consumed` bytes.
    Done(Incoming, usize),
    /// `more` bytes missing, if the parser knows.
    Incomplete(Option<usize>),
    /// Not RTSP.
    Malformed,
}

/// The read buffer of one connection and the messages it holds.
pub(crate) struct Framer {
    buf: BytesMut,
    max_request: usize,
    scan: Scan,
    /// Bytes examined so far, scans and parses together (tests check it
    /// stays linear in the input).
    #[cfg(test)]
    work: usize,
    /// Parse attempts so far.
    #[cfg(test)]
    parses: usize,
}

impl Framer {
    /// A framer refusing requests larger than `max_request` bytes.
    pub(crate) fn new(max_request: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            max_request,
            scan: Scan::Start,
            #[cfg(test)]
            work: 0,
            #[cfg(test)]
            parses: 0,
        }
    }

    /// The buffer's bound: a request of `max_request` bytes, or the largest
    /// interleaved frame, whichever is larger.
    pub(crate) fn limit(&self) -> usize {
        self.max_request.max(MAX_INTERLEAVED)
    }

    /// How many more bytes the buffer may take now.
    pub(crate) fn room(&self) -> usize {
        self.limit().saturating_sub(self.buf.len())
    }

    /// Whether part of a message is waiting for the rest of it. Meaningful
    /// once [`next`](Self::next) has returned `Ok(None)`: the line breaks
    /// some clients send between messages are dropped by then.
    pub(crate) fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Appends bytes read from the connection.
    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn count_parse(&mut self) {
        #[cfg(test)]
        {
            self.parses = self.parses.saturating_add(1);
        }
    }

    fn charge(&mut self, bytes: usize) {
        #[cfg(test)]
        {
            self.work = self.work.saturating_add(bytes);
        }
        let _ = bytes;
    }

    /// Takes the next complete message off the front of the buffer.
    /// `Ok(None)` means more bytes are needed; an error means the connection
    /// must be closed.
    pub(crate) fn next(&mut self) -> Result<Option<Incoming>, FramingError> {
        if self.scan == Scan::Start {
            // Some clients send bare line breaks as keep-alives.
            let blank = self.buf.iter().take_while(|b| matches!(b, b'\r' | b'\n')).count();
            self.charge(blank);
            self.buf.advance(blank);
            match self.buf.first() {
                None => return Ok(None),
                Some(b'$') => return Ok(self.next_interleaved()),
                Some(_) => self.scan = Scan::Head { scanned: 0, matched: 0, line_done: false },
            }
        }
        match self.scan {
            Scan::Start => Ok(None),
            Scan::Head { scanned, matched, line_done } => self.scan_head(scanned, matched, line_done),
            Scan::Body { head, needed } if self.buf.len() >= needed => self.finish(head),
            Scan::Body { .. } => Ok(None),
        }
    }

    fn next_interleaved(&mut self) -> Option<Incoming> {
        let &[_, channel, high, low] = self.buf.first_chunk::<INTERLEAVED_HEADER>()?;
        let len = usize::from(u16::from_be_bytes([high, low]));
        if self.buf.len() < INTERLEAVED_HEADER + len {
            return None;
        }
        self.buf.advance(INTERLEAVED_HEADER);
        let payload = self.buf.split_to(len).freeze();
        Some(Incoming::Data { channel, payload })
    }

    /// Examines the bytes not seen yet, looking for the end of the first line
    /// (which is checked at once) and then for the blank line ending the
    /// headers.
    fn scan_head(
        &mut self,
        from: usize,
        mut matched: u8,
        mut line_done: bool,
    ) -> Result<Option<Incoming>, FramingError> {
        let mut index = from;
        let mut head_end = None;
        let mut line_end = None;
        for &byte in self.buf.get(from..).unwrap_or_default() {
            index += 1;
            if !line_done && !first_line_byte(byte, matched) {
                return Err(FramingError::Malformed);
            }
            matched = match (matched, byte) {
                (0 | 2, b'\r') => matched + 1,
                (1 | 3, b'\n') => matched + 1,
                (_, b'\r') => 1,
                _ => 0,
            };
            if matched == 2 && !line_done {
                line_done = true;
                line_end = Some(index);
            }
            if matched == 4 {
                head_end = Some(index);
                break;
            }
        }
        self.charge(index.saturating_sub(from));
        if let Some(head) = head_end {
            return self.finish(head);
        }
        if let Some(line) = line_end {
            // The first line is complete: a line that is not RTSP is refused
            // now, not once the size limit is reached. The line alone is
            // judged: the headers read with it are judged with the whole
            // head, or a bad header would close the connection when it
            // arrives with the line and wait for the blank line when it
            // arrives after it.
            if let Attempt::Malformed = self.attempt(line) {
                return Err(FramingError::Malformed);
            }
        }
        if self.buf.len() >= self.max_request {
            return Err(FramingError::TooLarge);
        }
        self.scan = Scan::Head { scanned: index, matched, line_done };
        Ok(None)
    }

    /// Parses the message whose head is `head` bytes long, or decides how
    /// many bytes to wait for before trying again.
    fn finish(&mut self, head: usize) -> Result<Option<Incoming>, FramingError> {
        match self.attempt(self.buf.len()) {
            Attempt::Done(message, consumed) => {
                if consumed > self.max_request {
                    return Err(FramingError::TooLarge);
                }
                self.buf.advance(consumed.min(self.buf.len()));
                self.scan = Scan::Start;
                Ok(Some(message))
            }
            Attempt::Incomplete(more) => {
                let len = self.buf.len();
                if len >= self.max_request {
                    return Err(FramingError::TooLarge);
                }
                let needed = match more {
                    // A declared body that cannot fit is refused at once.
                    Some(more) => len.saturating_add(more),
                    // Unknown: try again once the buffer has doubled, which
                    // keeps the total work linear.
                    None => len.saturating_mul(2).min(self.max_request).max(len + 1),
                };
                if needed > self.max_request {
                    return Err(FramingError::TooLarge);
                }
                self.scan = Scan::Body { head, needed };
                Ok(None)
            }
            Attempt::Malformed => Err(FramingError::Malformed),
        }
    }

    /// One parse of the first `upto` bytes of the buffer by `rtsp-types`. It
    /// reads at most those, and only the message when the message is complete.
    fn attempt(&mut self, upto: usize) -> Attempt {
        self.count_parse();
        let bytes = self.buf.get(..upto).unwrap_or(self.buf.as_ref());
        let parsed = Message::<Vec<u8>>::parse(bytes);
        let read = match &parsed {
            Ok((_, consumed)) => *consumed,
            Err(_) => bytes.len(),
        };
        self.charge(read);
        match parsed {
            Ok((message, consumed)) => {
                let message = match message {
                    Message::Request(request) => Incoming::Request(request),
                    Message::Response(_) | Message::Data(_) => Incoming::Ignored,
                };
                Attempt::Done(message, consumed)
            }
            Err(ParseError::Incomplete(more)) => Attempt::Incomplete(more.map(std::num::NonZeroUsize::get)),
            Err(ParseError::Error) => Attempt::Malformed,
        }
    }
}

/// Whether `byte` may appear in a first line (request or status line), after
/// bytes matching `matched` characters of the line break. Control
/// characters are refused there, so binary garbage (a TLS hello) is refused at
/// once instead of filling the buffer; a line feed only ends the line. A tab
/// is whitespace a status line's reason phrase may hold (`rtsp-types` reads it
/// up to the line break).
fn first_line_byte(byte: u8, matched: u8) -> bool {
    match byte {
        b'\r' | b'\t' => true,
        b'\n' => matched == 1,
        0x7f => false,
        byte => byte >= 0x20,
    }
}

#[cfg(test)]
#[path = "framing_tests.rs"]
mod tests;
