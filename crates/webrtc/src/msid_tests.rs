//! The answer's stream and track ids, on text: what str0m writes, what an
//! offer can make it write, and any text at all.

use proptest::prelude::*;

use super::*;
use crate::fuzzing::answer_streams;

const STREAM: &str = "0123456789abcdef0123456789abcdef";

/// The lines of a str0m answer that matter here, among others that do not:
/// a stream per media line, the picture's SSRC and its retransmissions'.
const ANSWER: &str = "v=0\r\n\
o=str0m-0.25.0 1 2 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=msid-semantic: WMS picturestream soundstream\r\n\
a=ice-lite\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
a=msid:picturestream picturetrack\r\n\
a=rtcp-mux\r\n\
a=ssrc:11 cname:picturecname\r\n\
a=ssrc:11 msid:picturestream picturetrack\r\n\
a=ssrc:12 cname:picturecname\r\n\
a=ssrc:12 msid:picturestream picturetrack\r\n\
a=ssrc-group:FID 11 12\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:1\r\n\
a=sendonly\r\n\
a=msid:soundstream soundtrack\r\n\
a=rtcp-mux\r\n\
a=ssrc:21 cname:soundcname\r\n\
a=ssrc:21 msid:soundstream soundtrack\r\n";

#[test]
fn every_line_names_the_sessions_stream_and_a_track_of_its_own() {
    let expected = ANSWER
        .replace("WMS picturestream soundstream", &format!("WMS {STREAM}"))
        .replace("picturestream picturetrack", &format!("{STREAM} {STREAM}-0"))
        .replace("soundstream soundtrack", &format!("{STREAM} {STREAM}-1"));
    assert_eq!(one_stream(ANSWER, STREAM), expected);
    // Six lines named a stream, and only they changed.
    let changed = ANSWER.lines().zip(expected.lines()).filter(|(old, new)| old != new).count();
    assert_eq!(changed, 6);
}

#[test]
fn a_second_pass_changes_nothing() {
    let once = one_stream(ANSWER, STREAM);
    assert_eq!(one_stream(&once, STREAM), once);
}

#[test]
fn an_answer_that_names_no_stream_is_returned_as_it_is() {
    // Every media line refused: str0m then lists no stream either.
    let refused = "v=0\r\na=msid-semantic: WMS\r\nm=video 0 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\na=inactive\r\n";
    assert_eq!(one_stream(refused, STREAM), refused);
    assert_eq!(one_stream("", STREAM), "");
}

#[test]
fn line_endings_are_kept_as_they_are() {
    let unix = ANSWER.replace("\r\n", "\n");
    assert_eq!(one_stream(&unix, STREAM), one_stream(ANSWER, STREAM).replace("\r\n", "\n"));
    // A last line with no ending, and a stray carriage return.
    let cut = "m=audio 9 RTP/AVP 0\r\na=msid:s t\r\r\na=ssrc:7 msid:s t";
    assert_eq!(one_stream(cut, "x"), "m=audio 9 RTP/AVP 0\r\na=msid:x x-0\r\r\na=ssrc:7 msid:x x-0");
}

/// str0m hands back the `a=msid` of an offer's line, whatever follows the
/// stream id on it: nothing an offer wrote is left in the line.
#[test]
fn what_an_offer_wrote_is_replaced_whole() {
    let echoed = "a=msid-semantic: WMS theirs\r\nm=audio 9 RTP/AVP 0\r\na=msid:theirs a track with spaces\r\n\
                  a=ssrc:5 msid:theirs\r\na=ssrc:5 cname:theirs\r\n";
    let out = one_stream(echoed, "ours");
    assert_eq!(
        out,
        "a=msid-semantic: WMS ours\r\nm=audio 9 RTP/AVP 0\r\na=msid:ours ours-0\r\na=ssrc:5 msid:ours ours-0\r\n\
         a=ssrc:5 cname:theirs\r\n"
    );
}

/// A stream line in the session part belongs to no media line, and a
/// `msid-semantic` inside a media line is not the session's list: both are
/// left alone, as is an `a=ssrc` with no attribute.
#[test]
fn only_lines_in_their_place_are_rewritten() {
    let odd = "a=msid:early track\r\nm=video 9 RTP/AVP 96\r\na=msid-semantic: WMS inside\r\na=ssrc:9\r\na=msid:s t\r\n";
    assert_eq!(
        one_stream(odd, "x"),
        "a=msid:early track\r\nm=video 9 RTP/AVP 96\r\na=msid-semantic: WMS inside\r\na=ssrc:9\r\na=msid:x x-0\r\n"
    );
}

/// A carriage return alone ends no line: what follows it on a line that
/// names a stream goes with the line, and on any other line stays as it is.
/// Characters of several bytes are kept whole.
#[test]
fn a_lone_carriage_return_and_wide_characters() {
    let odd = "m=audio 9 RTP/AVP 0\r\na=msid:s t\ra=candidate:injected\r\na=x:\u{e9}t\u{e9}\ra=msid:u v\r\n\
               a=ssrc:7 msid:\u{1f3a5} \u{e9}\n";
    assert_eq!(
        one_stream(odd, "x"),
        "m=audio 9 RTP/AVP 0\r\na=msid:x x-0\r\na=x:\u{e9}t\u{e9}\ra=msid:u v\r\na=ssrc:7 msid:x x-0\n"
    );
    assert_eq!(answer_streams(odd.as_bytes()), Ok(()));
}

#[test]
fn a_stream_id_is_32_hex_characters_and_never_the_same_twice() {
    let (first, second) = (new_stream_id().unwrap(), new_stream_id().unwrap());
    for id in [&first, &second] {
        assert!(id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }
    assert_ne!(first, second);
}

#[test]
fn the_harness_takes_a_real_answer() {
    assert_eq!(answer_streams(ANSWER.as_bytes()), Ok(()));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn any_text_as_an_answer(text in "[ -~\r\n]{0,1500}") {
        prop_assert_eq!(answer_streams(text.as_bytes()), Ok(()));
    }

    /// Any bytes at all: what is not UTF-8 is not an answer, and the rest
    /// keeps the harness's promises.
    #[test]
    fn any_bytes_as_an_answer(data in proptest::collection::vec(any::<u8>(), 0..1500)) {
        prop_assert_eq!(answer_streams(&data), Ok(()));
    }

    /// The lines that name a stream among characters of any width and lone
    /// carriage returns: no line is cut inside a character.
    #[test]
    fn stream_lines_among_any_characters(text in "(m=\\PC{0,8}\r?\n|a=msid:\\PC{0,12}\r?\n|a=ssrc:\\PC{0,8} msid:\\PC{0,8}\r?\n|a=msid-semantic:\\PC{0,8}\n|\\PC{0,12}[\r\n]{0,2}){0,40}") {
        prop_assert_eq!(answer_streams(text.as_bytes()), Ok(()));
    }

    /// Answers close to str0m's: its lines in any order and number, with
    /// what an offer can put after an id.
    #[test]
    fn answers_close_to_str0ms(text in "(m=(video|audio) [09] UDP/TLS/RTP/SAVPF 96\r?\n|a=mid:[0-9a-z]{1,3}\r\n|a=msid-semantic:( WMS)?( [a-zA-Z0-9]{1,8}){0,3}\r\n|a=msid:[ -~]{0,20}\r?\n|a=ssrc:[0-9]{1,10}( (msid|cname):[ -~]{0,20})?\r\n|a=ssrc-group:FID 1 2\r\n|a=sendonly\r\n|[ -~]{0,12}\n){0,60}") {
        prop_assert_eq!(answer_streams(text.as_bytes()), Ok(()));
    }
}
