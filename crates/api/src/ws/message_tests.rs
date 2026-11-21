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
}

#[test]
fn later_types_and_unknown_ones() {
    for (name, later) in [("mse", Later::Mse), ("hls", Later::Hls), ("mjpeg", Later::Mjpeg), ("mp4", Later::Mp4)] {
        assert_eq!(parse(&format!(r#"{{"type":"{name}","value":"avc1.640029"}}"#)), Ok(Incoming::Later(later)));
        assert_eq!(later.name(), name);
    }
    assert_eq!(parse(r#"{"type":"foo","value":"bar"}"#), Ok(Incoming::Ignored));
    assert_eq!(parse(r#"{"type":"webrtc/answer","value":"v=0"}"#), Ok(Incoming::Ignored));
}

#[test]
fn refuses_what_is_not_a_message() {
    for text in ["", "{not json", "[]", "{}", r#"{"type":5}"#, r#"{"value":"x"}"#, "\"webrtc/offer\""] {
        assert_eq!(parse(text), Err(MessageError::NotMessage), "{text}");
    }
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
