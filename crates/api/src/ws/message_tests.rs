#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;

#[test]
fn reads_both_offer_dialects() {
    assert_eq!(
        parse(r#"{"type":"webrtc/offer","value":"v=0\r\n"}"#),
        Ok(Incoming::Offer { sdp: "v=0\r\n".into(), dialect: Dialect::Sdp })
    );
    // Home Assistant's library: an object, with its ICE servers (not used).
    let ha = r#"{"type":"webrtc","value":{"type":"offer","sdp":"v=0\r\n","ice_servers":[{"urls":["stun:x"]}]}}"#;
    assert_eq!(parse(ha), Ok(Incoming::Offer { sdp: "v=0\r\n".into(), dialect: Dialect::Object }));
}

#[test]
fn an_offer_without_an_offer_is_said_so_in_its_dialect() {
    for text in [
        r#"{"type":"webrtc/offer"}"#,
        r#"{"type":"webrtc/offer","value":12}"#,
        r#"{"type":"webrtc/offer","value":" "}"#,
    ] {
        assert!(matches!(parse(text), Ok(Incoming::BadOffer { dialect: Dialect::Sdp, .. })), "{text}");
    }
    for text in [
        r#"{"type":"webrtc","value":"v=0"}"#,
        r#"{"type":"webrtc","value":{"type":"answer","sdp":"v=0"}}"#,
        r#"{"type":"webrtc","value":{"type":"offer"}}"#,
        r#"{"type":"webrtc","value":{"type":"offer","sdp":7}}"#,
    ] {
        assert!(matches!(parse(text), Ok(Incoming::BadOffer { dialect: Dialect::Object, .. })), "{text}");
    }
}

#[test]
fn reads_candidates_and_their_end() {
    let line = "candidate:1 1 udp 2130706431 192.168.1.2 5000 typ host";
    assert_eq!(
        parse(&format!(r#"{{"type":"webrtc/candidate","value":"{line}"}}"#)),
        Ok(Incoming::Candidate(Some(line.into())))
    );
    assert_eq!(
        parse(&format!(r#"{{"type":"webrtc/candidate","value":"a={line}\r\n"}}"#)),
        Ok(Incoming::Candidate(Some(line.into())))
    );
    // A lone carriage return ends the line too (the proptest twin found it).
    assert_eq!(parse(r#"{"type":"webrtc/candidate","value":"0\r0"}"#), Ok(Incoming::Candidate(Some("0".into()))));
    // One line only: what follows it would be lines of the trickle fragment.
    assert_eq!(
        parse(&format!(
            r#"{{"type":"webrtc/candidate","value":"{line}\r\na=ice-ufrag:x\r\na=candidate:2 1 udp 1 1.2.3.4 9 typ host"}}"#
        )),
        Ok(Incoming::Candidate(Some(line.into())))
    );
    for end in [
        r#"{"type":"webrtc/candidate","value":""}"#,
        r#"{"type":"webrtc/candidate"}"#,
        r#"{"type":"webrtc/candidate","value":{"candidate":"x"}}"#,
    ] {
        assert_eq!(parse(end), Ok(Incoming::Candidate(None)), "{end}");
    }
    // A line still an attribute once its `a=` is taken is no candidate:
    // ignored, as go2rtc's ICE parser refuses it (the fuzz target's crash
    // of 2026-11-21: `a='` was trickled as `a=a='`).
    for doubled in [r#"{"type":"webrtc/candidate","value":"a=a='"}"#, r#"{"type":"webrtc/candidate","value":"a= a=x"}"#]
    {
        assert_eq!(parse(doubled), Ok(Incoming::Candidate(None)), "{doubled}");
    }
}

#[test]
fn later_types_and_unknown_ones() {
    // `mp4` reads its codec list as `mse` does (F2).
    assert_eq!(parse(r#"{"type":"mp4","value":"avc1.640029"}"#), Ok(Incoming::Mp4("avc1.640029".into())));
    assert_eq!(parse(r#"{"type":"mp4","value":7}"#), Ok(Incoming::Mp4(String::new())));
    // `hls` reads its codec list as `mse` does, and is answered its playlist.
    assert_eq!(
        parse(r#"{"type":"hls","value":"avc1.640029,mp4a.40.2"}"#),
        Ok(Incoming::Hls("avc1.640029,mp4a.40.2".into()))
    );
    assert_eq!(parse(r#"{"type":"hls"}"#), Ok(Incoming::Hls(String::new())));
    assert_eq!(Outgoing::Hls("#EXTM3U\n".into()).to_json(), r##"{"type":"hls","value":"#EXTM3U\n"}"##);
    // go2rtc's player sends `mjpeg` with no value; a value is not read.
    assert_eq!(parse(r#"{"type":"mjpeg"}"#), Ok(Incoming::Mjpeg));
    assert_eq!(parse(r#"{"type":"mjpeg","value":"x"}"#), Ok(Incoming::Mjpeg));
    assert_eq!(Outgoing::Mjpeg.to_json(), r#"{"type":"mjpeg"}"#);
    assert_eq!(parse(r#"{"type":"foo","value":"bar"}"#), Ok(Incoming::Ignored));
    assert_eq!(parse(r#"{"type":"webrtc/answer","value":"v=0"}"#), Ok(Incoming::Ignored));
}

#[test]
fn mse_carries_its_codec_list() {
    let list = "avc1.640029,hvc1.1.6.L153.B0,mp4a.40.2,flac,opus";
    assert_eq!(parse(&format!(r#"{{"type":"mse","value":"{list}"}}"#)), Ok(Incoming::Mse(list.to_owned())));
    // go2rtc reads a value that is no string as an empty list.
    for text in [r#"{"type":"mse"}"#, r#"{"type":"mse","value":""}"#, r#"{"type":"mse","value":["avc1"]}"#] {
        assert_eq!(parse(text), Ok(Incoming::Mse(String::new())), "{text}");
    }
    let answer = Outgoing::Mse(r#"video/mp4; codecs="avc1.64001f,mp4a.40.2""#.to_owned()).to_json();
    let back: serde_json::Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(back, serde_json::json!({"type": "mse", "value": "video/mp4; codecs=\"avc1.64001f,mp4a.40.2\""}));
}

#[test]
fn refuses_what_is_not_a_message() {
    for text in ["", "{not json", "[]", "{}", r#"{"type":5}"#, r#"{"value":"x"}"#, "\"webrtc/offer\""] {
        assert_eq!(parse(text), Err(MessageError::NotMessage), "{text}");
    }
    // An array is no message, however its items line up with `type` and
    // `value` (serde reads a struct from a sequence; go2rtc's decoder does
    // not: the fuzz target's crash of 2026-11-21, `["mp4",""]`).
    for text in [r#"["mp4",""]"#, r#"["webrtc/offer","v=0\r\n"]"#, r#"["mse"]"#] {
        assert_eq!(parse(text), Err(MessageError::NotMessage), "{text}");
    }
}

/// A frame is read whole or not at all: a lone surrogate escape is refused
/// in a field no handler reads as it is in `type` or `value` (the fuzz
/// target's crash of 2026-11-21: serde skipped it in an unknown field, so a
/// frame serde_json does not read as JSON was taken as a message).
#[test]
fn a_frame_that_is_not_json_whole_is_not_a_message() {
    for text in [
        r#"{"type":"mse","v$lue":"ty\udf76,f76,lac4."}"#,
        r#"{"type":"mse","value":"avc1.640029","x":{"y":["\ud800"]}}"#,
        r#"{"type":"mse","value":"\udf76"}"#,
        r#"{"type":"\ud800"}"#,
    ] {
        assert_eq!(parse(text), Err(MessageError::NotMessage), "{text}");
    }
    // A pair is a character, as anywhere.
    assert_eq!(parse(r#"{"type":"mse","x":"😀"}"#), Ok(Incoming::Mse(String::new())));
}

/// A key given twice is read at its last value, as go2rtc's decoder reads
/// it and as the fuzz harness's JSON does.
#[test]
fn a_key_given_twice_is_read_at_its_last_value() {
    assert_eq!(parse(r#"{"type":"hls","type":"mse","value":"flac"}"#), Ok(Incoming::Mse("flac".into())));
}

#[test]
fn writes_go2rtc_shapes() {
    let answer = Outgoing::Answer { sdp: "v=0\r\n".into(), dialect: Dialect::Sdp }.to_json();
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap(),
        serde_json::json!({"type":"webrtc/answer","value":"v=0\r\n"})
    );
    let answer = Outgoing::Answer { sdp: "v=0\r\n".into(), dialect: Dialect::Object }.to_json();
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap(),
        serde_json::json!({"type":"webrtc","value":{"type":"answer","sdp":"v=0\r\n"}})
    );
    let candidate = Outgoing::Candidate("candidate:1 1 udp 1 1.2.3.4 5 typ host".into()).to_json();
    assert_eq!(
        serde_json::from_str::<Value>(&candidate).unwrap(),
        serde_json::json!({"type":"webrtc/candidate","value":"candidate:1 1 udp 1 1.2.3.4 5 typ host"})
    );
    let error = Outgoing::Error { about: "webrtc/offer", reason: "stream not found".into() }.to_json();
    assert_eq!(
        serde_json::from_str::<Value>(&error).unwrap(),
        serde_json::json!({"type":"error","value":"webrtc/offer: stream not found"})
    );
}

#[test]
fn an_error_never_carries_a_credential() {
    let error =
        Outgoing::Error { about: "webrtc/offer", reason: "streams: rtsp://admin:hunter2@cam/live failed".into() };
    let text = error.to_json();
    assert!(!text.contains("hunter2"), "{text}");
    assert!(text.contains("webrtc/offer: streams: rtsp://"), "{text}");
}

#[test]
fn lists_the_candidates_of_an_answer_once() {
    let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host\r\n\
               a=candidate:2 1 tcp 1 127.0.0.1 5000 typ host tcptype passive\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
               a=candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host\r\na=end-of-candidates\r\n";
    assert_eq!(
        answer_candidates(sdp),
        vec![
            "candidate:1 1 udp 2130706431 127.0.0.1 5000 typ host".to_owned(),
            "candidate:2 1 tcp 1 127.0.0.1 5000 typ host tcptype passive".to_owned(),
        ]
    );
}
