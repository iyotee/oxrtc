use proptest::prelude::*;

use super::*;

/// A picture: starts and ends like a JPEG, `tag` makes it unique, `fill`
/// makes it long.
fn jpeg(tag: u8, fill: usize) -> Vec<u8> {
    let mut data = vec![0xff, 0xd8, tag];
    data.extend((0..fill).map(|i| (i % 200) as u8 + 1));
    data.extend_from_slice(&[0xff, 0xd9]);
    data
}

/// A part the way most cameras send it: delimiter, headers with the length,
/// the picture, a line break.
fn with_length(delimiter: &str, picture: &[u8]) -> Vec<u8> {
    let mut out =
        format!("{delimiter}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", picture.len()).into_bytes();
    out.extend_from_slice(picture);
    out.extend_from_slice(b"\r\n");
    out
}

/// Every part the parser gives for `chunks` fed in order.
fn parts(parser: &mut Parser, chunks: &[&[u8]]) -> Result<Vec<Vec<u8>>, MultipartError> {
    let mut out = Vec::new();
    for chunk in chunks {
        parser.push(chunk);
        while let Some(part) = parser.next_part()? {
            out.push(part.data.to_vec());
        }
    }
    Ok(out)
}

fn read(boundary: Option<&str>, stream: &[u8]) -> Vec<Vec<u8>> {
    parts(&mut Parser::new(boundary, 1 << 20), &[stream]).unwrap()
}

#[test]
fn parts_with_a_length_come_out_as_they_arrive() {
    let (a, b) = (jpeg(1, 3000), jpeg(2, 10));
    let stream = [with_length("--frame", &a), with_length("--frame", &b)].concat();
    assert_eq!(read(Some("frame"), &stream), vec![a.clone(), b.clone()]);
    // The second picture is out as soon as its last byte is: no delimiter after it.
    let mut parser = Parser::new(Some("frame"), 1 << 20);
    let got = parts(&mut parser, &[&stream[..stream.len() - 2]]).unwrap();
    assert_eq!(got, vec![a, b]);
}

#[test]
fn leading_dashes_of_the_declared_boundary_are_optional() {
    let (a, b) = (jpeg(1, 500), jpeg(2, 500));
    // `boundary=--frame` and parts delimited by `--frame` (many cameras)...
    let loose = [with_length("--frame", &a), with_length("--frame", &b)].concat();
    assert_eq!(read(Some("--frame"), &loose), vec![a.clone(), b.clone()]);
    // ...and by the strict `----frame`.
    let strict = [with_length("----frame", &a), with_length("----frame", &b)].concat();
    assert_eq!(read(Some("--frame"), &strict), vec![a.clone(), b.clone()]);
    // No length: the delimiter ends the part, dashes and line break off.
    let bare = |d: &str, p: &[u8]| {
        [format!("{d}\r\nContent-Type: image/jpeg\r\n\r\n").into_bytes(), p.to_vec(), b"\r\n".to_vec()].concat()
    };
    let strict = [bare("----frame", &a), bare("----frame", &b), bare("----frame", &a)].concat();
    assert_eq!(read(Some("--frame"), &strict), vec![a.clone(), b.clone()]);
    let loose = [bare("--frame", &a), bare("--frame", &b), bare("--frame", &a)].concat();
    assert_eq!(read(Some("--frame"), &loose), vec![a, b]);
}

#[test]
fn a_part_without_a_length_ends_at_the_next_delimiter() {
    let (a, b) = (jpeg(1, 2000), jpeg(2, 2000));
    let part =
        |p: &[u8]| [b"--ipcamera\r\nContent-Type: image/jpeg\r\n\r\n".to_vec(), p.to_vec(), b"\r\n".to_vec()].concat();
    let stream = [part(&a), part(&b)].concat();
    // The last one waits for its delimiter, a closing one included.
    assert_eq!(read(Some("ipcamera"), &stream), vec![a.clone()]);
    let closed = [stream, b"--ipcamera--\r\n".to_vec()].concat();
    let mut parser = Parser::new(Some("ipcamera"), 1 << 20);
    assert_eq!(parts(&mut parser, &[&closed]).unwrap(), vec![a, b]);
    assert!(parser.is_done());
}

#[test]
fn the_boundary_can_be_read_from_the_first_line() {
    let (a, b) = (jpeg(1, 100), jpeg(2, 100));
    let stream =
        [b"some preamble\r\n".to_vec(), with_length("--myboundary", &a), with_length("--myboundary", &b)].concat();
    assert_eq!(read(None, &stream), vec![a, b]);
    assert_eq!(read(Some(""), &[with_length("--x", &jpeg(1, 5))].concat()), vec![jpeg(1, 5)]);
}

#[test]
fn a_delimiter_with_spaces_and_dashes_of_its_own_is_taken_as_declared() {
    // D-Link: `boundary=--video boundary--`, parts delimited by `--video boundary--`.
    let (a, b) = (jpeg(1, 100), jpeg(2, 100));
    let stream = [with_length("--video boundary--", &a), with_length("--video boundary--", &b)].concat();
    assert_eq!(read(Some("--video boundary--"), &stream), vec![a, b]);
}

#[test]
fn line_feeds_alone_are_enough() {
    let a = jpeg(1, 100);
    let mut stream = b"--f\nContent-Type: image/jpeg\nContent-Length: ".to_vec();
    stream.extend_from_slice(format!("{}\n\n", a.len()).as_bytes());
    stream.extend_from_slice(&a);
    stream.extend_from_slice(b"\n--f\n\n");
    stream.extend_from_slice(&a);
    stream.extend_from_slice(b"\n--f\n");
    assert_eq!(read(Some("f"), &stream), vec![a.clone(), a]);
}

#[test]
fn junk_and_parts_that_are_not_pictures_are_dropped() {
    let a = jpeg(1, 100);
    let text = b"--f\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello\r\n".to_vec();
    let stream = [b"preamble".to_vec(), text, b"stray bytes\r\n".to_vec(), with_length("--f", &a)].concat();
    assert_eq!(read(Some("f"), &stream), vec![a]);
    // A part that lies about its length: the junk after it is dropped, the next one is read.
    let short = b"--f\r\nContent-Length: 4\r\n\r\n\xff\xd8abcdefgh\r\n".to_vec();
    let stream = [short, with_length("--f", &jpeg(9, 20))].concat();
    let got = read(Some("f"), &stream);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], b"\xff\xd8ab");
    assert_eq!(got[1], jpeg(9, 20));
}

#[test]
fn an_empty_part_is_not_a_picture() {
    let stream = [with_length("--f", b""), with_length("--f", &jpeg(3, 4))].concat();
    assert_eq!(read(Some("f"), &stream), vec![jpeg(3, 4)]);
}

#[test]
fn the_same_parts_whatever_the_cuts() {
    let (a, b, c) = (jpeg(1, 4000), jpeg(2, 0), jpeg(3, 1200));
    let stream = [
        with_length("--frame", &a),
        b"--frame\r\nContent-Type: image/jpeg\r\n\r\n".to_vec(),
        b.clone(),
        b"\r\n".to_vec(),
        with_length("--frame", &c),
        b"--frame--\r\n".to_vec(),
    ]
    .concat();
    let whole = read(Some("frame"), &stream);
    assert_eq!(whole, vec![a, b, c]);
    for size in [1, 2, 3, 7, 64, 1000] {
        let chunks: Vec<&[u8]> = stream.chunks(size).collect();
        assert_eq!(parts(&mut Parser::new(Some("frame"), 1 << 20), &chunks).unwrap(), whole, "chunks of {size}");
    }
}

#[test]
fn a_part_over_the_limit_is_an_error_not_memory() {
    // Declared length.
    let stream = with_length("--f", &jpeg(1, 5000));
    let mut parser = Parser::new(Some("f"), 1000);
    assert_eq!(parts(&mut parser, &[&stream]), Err(MultipartError::TooLarge(1000)));
    // No length: the delimiter never comes.
    let mut parser = Parser::new(Some("f"), 1000);
    parser.push(b"--f\r\nContent-Type: image/jpeg\r\n\r\n");
    let mut sent = 0;
    let error = loop {
        parser.push(&[0x55; 400]);
        sent += 400;
        match parser.next_part() {
            Ok(None) => assert!(parser.buffered() <= parser.limit()),
            Ok(Some(_)) => panic!("a part from nothing"),
            Err(e) => break e,
        }
        assert!(sent < 10_000, "no limit");
    };
    assert_eq!(error, MultipartError::TooLarge(1000));
}

/// Found by the `plausible_streams` property: a part with no length whose
/// delimiter arrived in the same read was handed out whatever its size; the
/// bound was only checked while the delimiter was still missing.
#[test]
fn a_part_without_a_length_over_the_limit_is_refused_when_its_delimiter_is_already_there() {
    let stream = [
        b"--f\r\nContent-Type: image/jpeg\r\n\r\n".to_vec(),
        jpeg(1, 600),
        b"\r\n--f\r\nContent-Type: image/jpeg\r\n\r\n".to_vec(),
        jpeg(2, 5),
        b"\r\n".to_vec(),
    ]
    .concat();
    let mut parser = Parser::new(Some("f"), 100);
    parser.push(&stream);
    assert_eq!(parser.next_part(), Err(MultipartError::TooLarge(100)));
    // The same stream within the bound.
    assert_eq!(read(Some("f"), &stream).len(), 1);
}

#[test]
fn headers_without_an_end_are_an_error() {
    let mut parser = Parser::new(Some("f"), 1000);
    parser.push(b"--f\r\n");
    let mut result = Ok(None);
    for _ in 0..100 {
        parser.push(
            b"X-Long-Header: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
        );
        result = parser.next_part();
        if result.is_err() {
            break;
        }
    }
    assert_eq!(result, Err(MultipartError::Malformed));
}

#[test]
fn garbage_without_a_delimiter_is_not_kept() {
    let mut parser = Parser::new(Some("boundary"), 1000);
    for _ in 0..1000 {
        parser.push(&[0x41; 1000]);
        assert_eq!(parser.next_part(), Ok(None));
        assert!(parser.buffered() <= "--boundary".len());
    }
    // Without a declared boundary: no line to take it from.
    let mut parser = Parser::new(None, 1000);
    for _ in 0..1000 {
        parser.push(&[0x41; 1000]);
        assert_eq!(parser.next_part(), Ok(None));
        assert!(parser.buffered() <= 1);
    }
    parser.push(b"--");
    parser.push(&[0x41; 300]);
    assert_eq!(parser.next_part(), Err(MultipartError::NoBoundary));
}

#[test]
fn a_delimiter_cut_in_two_is_still_found() {
    let a = jpeg(1, 100);
    let stream = with_length("--frame", &a);
    for cut in 1.."--frame".len() {
        let mut parser = Parser::new(Some("frame"), 1000);
        let got = parts(&mut parser, &[&stream[..cut], &stream[cut..]]).unwrap();
        assert_eq!(got, vec![a.clone()], "cut at {cut}");
    }
}

#[test]
fn content_types_are_read() {
    let body = |v: &str| body_of(Some(v));
    assert_eq!(body("multipart/x-mixed-replace; boundary=frame"), Body::Parts(Some("frame".into())));
    assert_eq!(body("multipart/x-mixed-replace;boundary=--myboundary"), Body::Parts(Some("--myboundary".into())));
    assert_eq!(body("Multipart/X-Mixed-Replace; Boundary=\"a b\"; charset=x"), Body::Parts(Some("a b".into())));
    assert_eq!(body("multipart/x-mixed-replace; boundary=frame; foo=bar"), Body::Parts(Some("frame".into())));
    assert_eq!(body("multipart/x-mixed-replace"), Body::Parts(None));
    assert_eq!(body("multipart/x-mixed-replace; boundary="), Body::Parts(None));
    assert_eq!(body("image/jpeg"), Body::Image);
    assert_eq!(body("IMAGE/JPG; q=1"), Body::Image);
    assert_eq!(body("application/octet-stream"), Body::Image);
    assert_eq!(body_of(None), Body::Image);
    assert_eq!(body("text/html; charset=utf-8"), Body::Other("text/html".into()));
    assert_eq!(body("text/\u{1b}[31mred"), Body::Other("text/?[31mred".into()));
    let long = format!("multipart/mixed; boundary={}", "a".repeat(300));
    assert_eq!(body(&long), Body::Parts(None), "a boundary of 300 bytes is no boundary");
}

proptest! {
    /// Any cut of a valid stream gives the parts of the whole stream.
    #[test]
    fn the_cuts_change_nothing(
        sizes in prop::collection::vec(0usize..3000, 1..6),
        cuts in prop::collection::vec(1usize..2500, 1..10),
        length in any::<bool>(),
    ) {
        let pictures: Vec<Vec<u8>> = sizes.iter().enumerate().map(|(i, n)| jpeg(i as u8, *n)).collect();
        let mut stream = Vec::new();
        for picture in &pictures {
            if length {
                stream.extend(with_length("--frame", picture));
            } else {
                stream.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n");
                stream.extend_from_slice(picture);
                stream.extend_from_slice(b"\r\n");
            }
        }
        stream.extend_from_slice(b"--frame--\r\n");
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut rest = stream.as_slice();
        for cut in cuts.iter().cycle() {
            if rest.is_empty() { break; }
            let (head, tail) = rest.split_at((*cut).min(rest.len()));
            chunks.push(head);
            rest = tail;
        }
        let got = parts(&mut Parser::new(Some("frame"), 1 << 20), &chunks).unwrap();
        prop_assert_eq!(got, pictures);
    }
}
