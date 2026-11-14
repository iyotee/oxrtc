//! Message framing on the RTSP connection: requests (parsed by `rtsp-types`)
//! and RTP/RTCP interleaved in `$`-prefixed frames, from one bounded buffer.

use bytes::{Buf, Bytes, BytesMut};
use rtsp_types::headers::{CSEQ, SERVER};
use rtsp_types::{Message, ParseError, Request, Response, StatusCode, Version};

/// An interleaved frame: `$`, channel, 16-bit length, then the packet.
const INTERLEAVED_HEADER: usize = 4;

/// Largest interleaved frame: the length field is 16 bits.
pub(crate) const MAX_INTERLEAVED: usize = INTERLEAVED_HEADER + u16::MAX as usize;

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

/// The read buffer's bound: a request of `max_request` bytes, or the largest
/// interleaved frame, whichever is larger.
pub(crate) fn buffer_limit(max_request: usize) -> usize {
    max_request.max(MAX_INTERLEAVED)
}

/// Takes the next complete message off the front of `buf`. `Ok(None)` means
/// more bytes are needed; an error means the connection must be closed.
pub(crate) fn next_message(buf: &mut BytesMut, max_request: usize) -> Result<Option<Incoming>, FramingError> {
    // Some clients send bare line breaks as keep-alives.
    let blank = buf.iter().take_while(|b| matches!(b, b'\r' | b'\n')).count();
    buf.advance(blank);
    match buf.first() {
        None => Ok(None),
        Some(b'$') => Ok(next_interleaved(buf)),
        Some(_) => next_request(buf, max_request),
    }
}

fn next_interleaved(buf: &mut BytesMut) -> Option<Incoming> {
    let &[_, channel, high, low] = buf.first_chunk::<INTERLEAVED_HEADER>()?;
    let len = usize::from(u16::from_be_bytes([high, low]));
    if buf.len() < INTERLEAVED_HEADER + len {
        return None;
    }
    buf.advance(INTERLEAVED_HEADER);
    let payload = buf.split_to(len).freeze();
    Some(Incoming::Data { channel, payload })
}

fn next_request(buf: &mut BytesMut, max_request: usize) -> Result<Option<Incoming>, FramingError> {
    match Message::<Vec<u8>>::parse(buf.as_ref()) {
        Ok((message, consumed)) => {
            if consumed > max_request {
                return Err(FramingError::TooLarge);
            }
            buf.advance(consumed.min(buf.len()));
            Ok(Some(match message {
                Message::Request(request) => Incoming::Request(request),
                Message::Response(_) | Message::Data(_) => Incoming::Ignored,
            }))
        }
        Err(ParseError::Incomplete(_)) if buf.len() >= max_request => Err(FramingError::TooLarge),
        Err(ParseError::Incomplete(_)) => Ok(None),
        Err(ParseError::Error) => Err(FramingError::Malformed),
    }
}

/// A response to `request` with its `CSeq` echoed.
pub(crate) fn reply(request: &Request<Vec<u8>>, status: StatusCode) -> rtsp_types::ResponseBuilder {
    let builder = Response::builder(Version::V1_0, status).header(SERVER, "oxrtc");
    match request.header(&CSEQ) {
        Some(cseq) => builder.header(CSEQ, cseq.as_str()),
        None => builder,
    }
}

/// A response for a request that could not be parsed.
pub(crate) fn bare(status: StatusCode) -> Bytes {
    encode(&Response::builder(Version::V1_0, status).header(SERVER, "oxrtc").build(Vec::new()))
}

/// Serializes a response.
pub(crate) fn encode(response: &Response<Vec<u8>>) -> Bytes {
    let mut out = Vec::with_capacity(256);
    // Writing into a Vec cannot fail for want of space.
    let _ = response.write(&mut out);
    Bytes::from(out)
}

/// Appends an interleaved frame header for a packet of `len` bytes. Refuses a
/// packet the 16-bit length cannot describe.
pub(crate) fn interleaved_header(out: &mut BytesMut, channel: u8, len: usize) -> bool {
    let Ok(len) = u16::try_from(len) else { return false };
    out.extend_from_slice(&[b'$', channel]);
    out.extend_from_slice(&len.to_be_bytes());
    true
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn parse_all(bytes: &[u8], max: usize) -> (Vec<Incoming>, Result<(), FramingError>) {
        let mut buf = BytesMut::from(bytes);
        let mut out = Vec::new();
        loop {
            match next_message(&mut buf, max) {
                Ok(Some(message)) => out.push(message),
                Ok(None) => return (out, Ok(())),
                Err(error) => return (out, Err(error)),
            }
        }
    }

    #[test]
    fn requests_and_interleaved_frames_in_one_buffer() {
        let mut bytes = b"OPTIONS rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec();
        bytes.extend_from_slice(&[b'$', 1, 0, 3, 9, 8, 7]);
        bytes.extend_from_slice(b"\r\nGET_PARAMETER rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        let (messages, end) = parse_all(&bytes, 4096);
        assert_eq!(end, Ok(()));
        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[1], Incoming::Data { channel: 1, payload } if payload[..] == [9, 8, 7]));
    }

    #[test]
    fn partial_input_waits_for_more() {
        let full = b"DESCRIBE rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 3\r\n\r\n";
        for cut in 0..full.len() {
            let (messages, end) = parse_all(&full[..cut], 4096);
            assert!(messages.is_empty() && end.is_ok(), "cut at {cut}");
        }
        let (messages, _) = parse_all(&[b'$', 0, 0, 10, 1, 2], 4096);
        assert!(messages.is_empty());
    }

    #[test]
    fn oversized_request_is_refused() {
        let mut bytes = b"ANNOUNCE rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\nX-Pad: ".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', 5000));
        let (_, end) = parse_all(&bytes, 1024);
        assert_eq!(end, Err(FramingError::TooLarge));
        // A declared body beyond the limit is refused once the buffer is full.
        let mut bytes = b"ANNOUNCE rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 999999\r\n\r\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'v', 2048));
        let (_, end) = parse_all(&bytes, 1024);
        assert_eq!(end, Err(FramingError::TooLarge));
    }

    #[test]
    fn garbage_is_malformed() {
        let (_, end) = parse_all(b"\x16\x03\x01\x02\x00 not rtsp at all\r\n\r\n", 4096);
        assert_eq!(end, Err(FramingError::Malformed));
    }

    #[test]
    fn reply_echoes_cseq() {
        let (messages, _) = parse_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 42\r\n\r\n", 4096);
        let Incoming::Request(request) = &messages[0] else { panic!("not a request") };
        let text = encode(&reply(request, StatusCode::Ok).build(Vec::new()));
        let text = std::str::from_utf8(&text).unwrap();
        // rtsp-types writes "Ok" as the reason phrase; clients read the code.
        assert!(text.starts_with("RTSP/1.0 200 "), "{text}");
        assert!(text.contains("CSeq: 42\r\n"), "{text}");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// Random bytes never panic the parser, and it never keeps more than
        /// its bound.
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..3000), max in 16usize..2048) {
            let (_, _) = parse_all(&bytes, max);
        }

        /// Random bytes behind a valid request prefix: same.
        #[test]
        fn random_tails_never_panic(tail in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let mut bytes = b"SETUP rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\n".to_vec();
            bytes.extend(tail);
            let (_, _) = parse_all(&bytes, 1024);
        }
    }
}
