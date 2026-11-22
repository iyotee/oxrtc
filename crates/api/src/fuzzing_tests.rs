//! The stable twins of the fuzz targets: the same harnesses, under proptest.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use proptest::prelude::*;

use super::*;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(512)
}

/// Values a client sends: SDP-like text, candidate lines, line ends, `a=`.
fn value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\na=candidate:1 1 udp 1 1.2.3.4 9 typ host\r\n".to_owned()),
        Just("candidate:1 1 udp 2130706431 192.168.1.2 5000 typ host".to_owned()),
        Just("a=candidate:1 1 udp 1 1.2.3.4 9 typ host\r\na=ice-ufrag:x".to_owned()),
        // `mse` codec lists: go2rtc's player's, and pieces of them.
        Just("avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus".to_owned()),
        "((avc1|avc3|hvc1|hev1|HEV1|mp4a|opus|flac|vp09)(\\.[0-9A-Za-z]{0,6}){0,3}[, ]{0,2}){0,6}",
        "[ -~\\r\\n\\t]{0,80}",
    ]
}

fn kind() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("webrtc/offer".to_owned()),
        Just("webrtc".to_owned()),
        Just("webrtc/candidate".to_owned()),
        Just("mse".to_owned()),
        Just("hls".to_owned()),
        Just("mjpeg".to_owned()),
        Just("mp4".to_owned()),
        "[a-z/]{0,12}",
    ]
}

/// A message as a client could write it: the value a string, an offer
/// object, a number or absent; a string with a field no handler reads,
/// holding a `\u` escape written by hand, a lone surrogate half the time
/// (the fuzz target's crash of 2026-11-21, which `json!` never writes); or
/// the type and the value as an array (its third crash).
fn message() -> impl Strategy<Value = String> {
    let escape = prop_oneof![0xd800u16..0xe000, any::<u16>()];
    (kind(), value(), 0u8..6, escape).prop_map(|(kind, value, shape, escape)| {
        let value = match shape {
            0 => serde_json::json!(value),
            1 => serde_json::json!({ "type": "offer", "sdp": value, "ice_servers": [] }),
            2 => serde_json::json!(7),
            3 => return serde_json::json!({ "type": kind }).to_string(),
            4 => return serde_json::json!([kind, value]).to_string(),
            _ => {
                let (kind, value) = (serde_json::json!(kind), serde_json::json!(value));
                return format!(r#"{{"type":{kind},"value":{value},"extra":"\u{escape:04x}"}}"#);
            }
        };
        serde_json::json!({ "type": kind, "value": value }).to_string()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Any bytes.
    #[test]
    fn ws_random_bytes(data in prop::collection::vec(any::<u8>(), 0..400)) {
        prop_assert_eq!(ws_message(&data), Ok(()));
    }

    /// Plausible messages, and the same with bytes flipped.
    #[test]
    fn ws_plausible_messages(text in message(), flip in prop::option::of((any::<prop::sample::Index>(), any::<u8>()))) {
        let mut data = text.into_bytes();
        if let Some((at, byte)) = flip
            && !data.is_empty()
        {
            let at = at.index(data.len());
            data[at] = byte;
        }
        prop_assert_eq!(ws_message(&data), Ok(()));
    }
}

/// The committed seeds of the fuzz target pass.
#[test]
fn ws_message_seeds_pass() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/ws_message");
    let mut replayed = 0;
    for entry in std::fs::read_dir(&seeds).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        assert_eq!(ws_message(&data), Ok(()));
        replayed += 1;
    }
    assert!(replayed >= 4, "only {replayed} seeds under {}", seeds.display());
}

/// Queries of `stream.mp4` as clients write them: the keys go2rtc reads,
/// codec names right and wrong, durations, and names to save under with
/// quotes, line ends, slashes and characters that are not ASCII.
fn mp4_query_text() -> impl Strategy<Value = String> {
    let key = prop_oneof![
        Just("src"),
        Just("mp4"),
        Just("video"),
        Just("audio"),
        Just("duration"),
        Just("filename"),
        Just("queue"),
        Just("rotate"),
    ];
    let value = prop_oneof![
        Just(String::new()),
        Just("flac".to_owned()),
        Just("all".to_owned()),
        "(h264|h265|hevc|aac|opus|pcma|pcmu|l16|jpeg|copy|any|vp9|mp3)(,(h264|aac|opus|pcma)){0,2}",
        "[0-9]{1,11}",
        "-?[0-9]{0,3}\\.[0-9]{0,2}",
        "[ -~]{0,40}",
        "(%22|%0D%0A|%2F|%5C|%00|%CF%80|%C3%A9|%FF|[a-z. ]){0,30}",
        "\\PC{0,200}",
    ];
    prop::collection::vec((key, value), 0..8).prop_map(|pairs| {
        pairs
            .iter()
            .map(|(k, v)| if v.is_empty() { (*k).to_owned() } else { format!("{k}={v}") })
            .collect::<Vec<_>>()
            .join("&")
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Any bytes.
    #[test]
    fn mp4_query_random_bytes(data in prop::collection::vec(any::<u8>(), 0..300)) {
        prop_assert_eq!(mp4_query(&data), Ok(()));
    }

    /// Plausible queries.
    #[test]
    fn mp4_query_plausible(query in mp4_query_text()) {
        prop_assert_eq!(mp4_query(query.as_bytes()), Ok(()));
    }
}

/// The committed seeds of the fuzz target pass.
#[test]
fn mp4_query_seeds_pass() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/mp4_query");
    let mut replayed = 0;
    for entry in std::fs::read_dir(&seeds).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        assert_eq!(mp4_query(&data), Ok(()));
        replayed += 1;
    }
    assert!(replayed >= 4, "only {replayed} seeds under {}", seeds.display());
}
