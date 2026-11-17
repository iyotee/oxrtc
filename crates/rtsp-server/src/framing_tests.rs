use proptest::prelude::*;
use rtsp_types::{Method, StatusCode};

use super::*;
use crate::wire::{encode, reply};

/// Feeds `bytes` in chunks of `chunk` bytes (as reads would deliver them) and
/// collects every message, stopping at the first error.
fn feed(framer: &mut Framer, bytes: &[u8], chunk: usize) -> (Vec<Incoming>, Result<(), FramingError>) {
    let mut out = Vec::new();
    for piece in bytes.chunks(chunk.max(1)) {
        framer.extend(piece);
        loop {
            match framer.next() {
                Ok(Some(message)) => out.push(message),
                Ok(None) => break,
                Err(error) => return (out, Err(error)),
            }
        }
    }
    (out, Ok(()))
}

fn parse_all(bytes: &[u8], max: usize) -> (Vec<Incoming>, Result<(), FramingError>) {
    feed(&mut Framer::new(max), bytes, bytes.len())
}

/// A valid request of about `size` bytes: padding headers, then a body.
fn big_request(size: usize, body: usize) -> Vec<u8> {
    let mut bytes = b"ANNOUNCE rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 7\r\n".to_vec();
    let mut n = 0;
    while bytes.len() + body + 64 < size {
        bytes.extend_from_slice(format!("X-Pad-{n}: {}\r\n", "p".repeat(40)).as_bytes());
        n += 1;
    }
    bytes.extend_from_slice(format!("Content-Length: {body}\r\n\r\n").as_bytes());
    bytes.extend(std::iter::repeat_n(b'b', body));
    bytes
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
    // A declared body beyond the limit is refused.
    let mut bytes = b"ANNOUNCE rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 999999\r\n\r\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'v', 2048));
    let (_, end) = parse_all(&bytes, 1024);
    assert_eq!(end, Err(FramingError::TooLarge));
}

#[test]
fn the_size_limit_is_the_same_whole_or_byte_by_byte() {
    // Exactly at the limit: accepted; one byte over: refused - in one read or
    // in one read per byte.
    let at = big_request(2048, 100);
    let limit = at.len();
    for chunk in [at.len(), 1, 7] {
        let (messages, end) = feed(&mut Framer::new(limit), &at, chunk);
        assert_eq!((messages.len(), end), (1, Ok(())), "chunk {chunk}");
        let (messages, end) = feed(&mut Framer::new(limit - 1), &at, chunk);
        assert_eq!((messages.len(), end), (0, Err(FramingError::TooLarge)), "chunk {chunk}");
    }
}

#[test]
fn headers_without_an_end_are_refused_at_the_limit() {
    let mut framer = Framer::new(1024);
    let mut bytes = b"OPTIONS * RTSP/1.0\r\n".to_vec();
    while bytes.len() < 4096 {
        bytes.extend_from_slice(b"A: b\r\n");
    }
    let (_, end) = feed(&mut framer, &bytes, 1);
    assert_eq!(end, Err(FramingError::TooLarge));
}

#[test]
fn garbage_is_malformed() {
    let (_, end) = parse_all(b"\x16\x03\x01\x02\x00 not rtsp at all\r\n\r\n", 4096);
    assert_eq!(end, Err(FramingError::Malformed));
}

#[test]
fn binary_garbage_is_refused_at_once_not_at_the_limit() {
    // A TLS ClientHello has no line break: it must not sit in the buffer
    // until the size limit.
    let mut framer = Framer::new(64 * 1024);
    let (_, end) = feed(&mut framer, b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03", 1);
    assert_eq!(end, Err(FramingError::Malformed));
    // A bad request line is refused once the line is complete.
    let mut framer = Framer::new(64 * 1024);
    let (_, end) = feed(&mut framer, b"hello there\r\n", 1);
    assert_eq!(end, Err(FramingError::Malformed));
}

/// Found by the fuzzer (`fuzz/seeds/rtsp_request/crash-5d04e890`): a header
/// that is not one closed the connection when it arrived in the same read as
/// its first line, and waited for the end of the headers when it arrived
/// after it. The line alone is judged at its end, the headers with the head.
#[test]
fn a_bad_header_is_judged_with_the_head_however_it_arrives() {
    let mut unfinished = b"SETUP rtsp://127.0.0.1/x RTSP/1.0\r\n".to_vec();
    unfinished.extend_from_slice(b"CSeq: 3\r\nTranspo?\x00\x07 bad");
    for chunk in [1, 7, unfinished.len()] {
        let (messages, end) = feed(&mut Framer::new(4096), &unfinished, chunk);
        assert!(messages.is_empty() && end == Ok(()), "reads of {chunk}: {end:?}");
    }
    let mut finished = unfinished.clone();
    finished.extend_from_slice(b"\r\n\r\n");
    for chunk in [1, 7, finished.len()] {
        let (_, end) = feed(&mut Framer::new(4096), &finished, chunk);
        assert_eq!(end, Err(FramingError::Malformed), "reads of {chunk}");
    }
    // A first line that is not RTSP is still refused at its end.
    for chunk in [1, 7, 64] {
        let (_, end) = feed(&mut Framer::new(4096), b"GARBAGE LINE\r\nCSeq: 1\r\n", chunk);
        assert_eq!(end, Err(FramingError::Malformed), "reads of {chunk}");
    }
}

#[test]
fn a_tab_in_a_status_line_is_not_binary_garbage() {
    // rtsp-types reads a reason phrase up to the line break, tabs included:
    // such a response is ignored, as before the framing was incremental, not
    // refused as garbage.
    for chunk in [1, 64] {
        let (messages, end) = feed(&mut Framer::new(4096), b"RTSP/1.0 200 O\tK\r\nCSeq: 1\r\n\r\n", chunk);
        assert_eq!(end, Ok(()), "chunk {chunk}");
        assert!(matches!(messages.as_slice(), [Incoming::Ignored]), "chunk {chunk}: {messages:?}");
    }
}

#[test]
fn a_request_fed_byte_by_byte_is_the_same_request() {
    let mut bytes = b"\r\nSETUP rtsp://127.0.0.1:8554/cam/trackID=0 RTSP/1.0\r\nCSeq: 4\r\n".to_vec();
    bytes.extend_from_slice(b"Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nContent-Length: 5\r\n\r\nhello");
    bytes.extend_from_slice(&[b'$', 2, 0, 2, 0xAA, 0xBB]);
    bytes.extend_from_slice(b"TEARDOWN rtsp://127.0.0.1:8554/cam RTSP/1.0\r\nCSeq: 5\r\n\r\n");
    let (whole, end) = parse_all(&bytes, 4096);
    assert_eq!(end, Ok(()));
    let (single, end) = feed(&mut Framer::new(4096), &bytes, 1);
    assert_eq!(end, Ok(()));
    assert_eq!(single.len(), 3);
    assert_eq!(format!("{whole:?}"), format!("{single:?}"));
    let Incoming::Request(setup) = &single[0] else { panic!("not a request: {:?}", single[0]) };
    assert_eq!(setup.method(), Method::Setup);
    assert_eq!(setup.body(), b"hello");
    assert!(matches!(&single[1], Incoming::Data { channel: 2, payload } if payload[..] == [0xAA, 0xBB]));
    let Incoming::Request(teardown) = &single[2] else { panic!("not a request") };
    assert_eq!(teardown.method(), Method::Teardown);
}

/// R10: a slow sender (one byte per read) must cost work proportional to
/// what it sends, not to its square. Reparsing the whole buffer on every read
/// examines about n^2/2 bytes for an n-byte request.
#[test]
fn work_stays_linear_on_a_slow_sender() {
    for size in [4 * 1024, 16 * 1024, 60 * 1024] {
        let bytes = big_request(size, 512);
        let mut framer = Framer::new(64 * 1024);
        let (messages, end) = feed(&mut framer, &bytes, 1);
        assert_eq!((messages.len(), end), (1, Ok(())), "size {size}");
        let n = bytes.len();
        assert!(framer.work <= 4 * n, "size {n}: {} bytes examined, more than 4n", framer.work);
        // First line, headers, body: three parses, whatever the read count.
        assert!(framer.parses <= 3, "size {n}: {} parse attempts", framer.parses);
    }
    // And a request that never ends, up to the limit: same.
    let mut framer = Framer::new(64 * 1024);
    let mut endless = b"OPTIONS * RTSP/1.0\r\n".to_vec();
    while endless.len() < 70 * 1024 {
        endless.extend_from_slice(b"X-Slow: 1\r\n");
    }
    let (_, end) = feed(&mut framer, &endless, 1);
    assert_eq!(end, Err(FramingError::TooLarge));
    assert!(framer.work <= 4 * 64 * 1024, "{} bytes examined", framer.work);
    assert_eq!(framer.parses, 1, "only the first line is parsed before the headers end");
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

    /// Random bytes never panic the framer, whatever the read sizes.
    #[test]
    fn random_bytes_never_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..3000),
        max in 16usize..2048,
        chunk in 1usize..64,
    ) {
        let (_, _) = feed(&mut Framer::new(max), &bytes, chunk);
    }

    /// Random bytes behind a valid request prefix: same.
    #[test]
    fn random_tails_never_panic(tail in proptest::collection::vec(any::<u8>(), 0..2000), chunk in 1usize..64) {
        let mut bytes = b"SETUP rtsp://127.0.0.1/x RTSP/1.0\r\nCSeq: 1\r\n".to_vec();
        bytes.extend(tail);
        let (_, _) = feed(&mut Framer::new(1024), &bytes, chunk);
    }

    /// Valid traffic split at random gives the same messages as in one read.
    #[test]
    fn any_split_gives_the_same_messages(chunk in 1usize..200, body in 0usize..300) {
        let mut bytes = big_request(700, body);
        bytes.extend_from_slice(&[b'$', 0, 0, 1, 5]);
        bytes.extend_from_slice(b"\r\n\r\nOPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        let (whole, end) = parse_all(&bytes, 4096);
        prop_assert_eq!(end, Ok(()));
        let (split, end) = feed(&mut Framer::new(4096), &bytes, chunk);
        prop_assert_eq!(end, Ok(()));
        prop_assert_eq!(format!("{whole:?}"), format!("{split:?}"));
    }
}

/// The framing as it was before R10: the whole buffer reparsed on every read.
/// Kept as the oracle the incremental framer must agree with.
fn reference_next(buf: &mut BytesMut, max_request: usize) -> Result<Option<Incoming>, FramingError> {
    let blank = buf.iter().take_while(|b| matches!(b, b'\r' | b'\n')).count();
    buf.advance(blank);
    if buf.first() == Some(&b'$') {
        let Some(&[_, channel, high, low]) = buf.first_chunk::<INTERLEAVED_HEADER>() else { return Ok(None) };
        let len = usize::from(u16::from_be_bytes([high, low]));
        if buf.len() < INTERLEAVED_HEADER + len {
            return Ok(None);
        }
        buf.advance(INTERLEAVED_HEADER);
        return Ok(Some(Incoming::Data { channel, payload: buf.split_to(len).freeze() }));
    }
    if buf.is_empty() {
        return Ok(None);
    }
    match Message::<Vec<u8>>::parse(buf.as_ref()) {
        Ok((_, consumed)) if consumed > max_request => Err(FramingError::TooLarge),
        Ok((message, consumed)) => {
            buf.advance(consumed);
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

fn reference_all(bytes: &[u8], max: usize) -> (Vec<Incoming>, Result<(), FramingError>) {
    let mut buf = BytesMut::new();
    let mut out = Vec::new();
    // As the connection does: never more than the buffer's bound.
    let limit = Framer::new(max).limit();
    for &byte in bytes {
        if buf.len() >= limit {
            break;
        }
        buf.extend_from_slice(&[byte]);
        loop {
            match reference_next(&mut buf, max) {
                Ok(Some(message)) => out.push(message),
                Ok(None) => break,
                Err(error) => return (out, Err(error)),
            }
        }
    }
    (out, Ok(()))
}

/// Fragments of RTSP traffic, valid and not, to splice together.
fn fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec()),
        Just(b"DESCRIBE rtsp://h/x RTSP/1.0\r\nCSeq: 2\r\nX-Folded: a\r\n b\r\n\r\n".to_vec()),
        Just(b"ANNOUNCE rtsp://h/x RTSP/1.0\r\nContent-Length: 4\r\n\r\nabcd".to_vec()),
        Just(b"RTSP/1.0 200 OK\r\nCSeq: 3\r\n\r\n".to_vec()),
        Just(b"RTSP/1.0 200 O\tK\r\n\r\n".to_vec()),
        Just(vec![b'$', 0, 0, 2, 1, 2]),
        Just(b"\r\n".to_vec()),
        Just(b"\r".to_vec()),
        Just(b"\n".to_vec()),
        Just(b" ".to_vec()),
        Just(b"\t".to_vec()),
        Just(b":".to_vec()),
        proptest::collection::vec(any::<u8>(), 0..6),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// The incremental framer gives the messages the whole-buffer reparse
    /// gave, whatever the read sizes. It may refuse a broken stream earlier
    /// (a control byte in a first line, a declared body that cannot fit) but
    /// never refuses a message the reparse accepted, nor yields one after
    /// the reparse refused.
    #[test]
    fn the_framer_agrees_with_the_whole_buffer_reparse(
        fragments in proptest::collection::vec(fragment(), 0..12),
        chunk in 1usize..40,
        max in 20usize..300,
    ) {
        let bytes = fragments.concat();
        let (reference, reference_end) = reference_all(&bytes, max);
        let (framed, framed_end) = feed(&mut Framer::new(max), &bytes, chunk);
        let common = reference.len().min(framed.len());
        prop_assert_eq!(format!("{:?}", reference.get(..common)), format!("{:?}", framed.get(..common)));
        match (reference_end, framed_end) {
            (Ok(()), Ok(())) => prop_assert_eq!(reference.len(), framed.len()),
            (Err(_), Ok(())) => prop_assert!(framed.len() <= reference.len()),
            (Ok(()), Err(_)) => prop_assert!(reference.len() <= framed.len(), "refused a message the reparse accepted"),
            // Both refuse the same message, perhaps for another reason (a line
            // too long for the limit is TooLarge before it can be Malformed).
            (Err(_), Err(_)) => prop_assert_eq!(reference.len(), framed.len()),
        }
    }
}
