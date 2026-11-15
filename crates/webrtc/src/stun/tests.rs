#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::net::SocketAddr;

use super::message::StunError;
use super::*;

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
}

fn hmac() -> Hmac {
    str0m::crypto::from_feature_flags().sha1_hmac_provider
}

/// RFC 5769 section 2.1: a request with a short-term credential and a fingerprint.
const SAMPLE_REQUEST: &str = "00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
    80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
    00 24 00 04 6e 00 01 ff 80 29 00 08 93 2f f9 b1 51 26 3b 36
    00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
    00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
    80 28 00 04 e5 7a 3b cf";

/// RFC 5769 section 2.2: an IPv4 response, XOR-MAPPED-ADDRESS 192.0.2.1:32853.
const SAMPLE_RESPONSE: &str = "01 01 00 3c 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
    80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
    00 20 00 08 00 01 a1 47 e1 12 a6 43
    00 08 00 14 2b 91 f5 99 fd 9e 90 c3 8c 74 89 f9 2a f9 ba 53 f0 6b e7 d7
    80 28 00 04 c0 7d 4c 96";

/// RFC 5769 section 2.4: a request with a long-term credential
/// (user "マトリックス", realm "example.org", password "TheMatrIX").
const LONG_TERM_REQUEST: &str = "00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
    00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
    00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41
    00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
    00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66";

#[test]
fn rfc5769_short_term_request() {
    let data = hex(SAMPLE_REQUEST);
    let message = Message::parse(&data).unwrap();
    assert_eq!((message.class, message.method), (Class::Request, method::BINDING));
    assert!(message.fingerprint_ok());
    assert!(message.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt", hmac()));
    assert!(!message.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBu", hmac()));
    assert_eq!(message.text(attr::USERNAME, 64), Some("evtj:h6vY"));
}

#[test]
fn rfc5769_response_address() {
    let data = hex(SAMPLE_RESPONSE);
    let message = Message::parse(&data).unwrap();
    assert_eq!(message.class, Class::Success);
    assert!(message.fingerprint_ok());
    assert!(message.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt", hmac()));
    let expected: SocketAddr = "192.0.2.1:32853".parse().unwrap();
    assert_eq!(message.xor_address(attr::XOR_MAPPED_ADDRESS), Some(expected));
}

#[test]
fn rfc5769_long_term_credential() {
    let data = hex(LONG_TERM_REQUEST);
    let message = Message::parse(&data).unwrap();
    assert_eq!(message.text(attr::REALM, 128), Some("example.org"));
    assert_eq!(message.text(attr::NONCE, 128), Some("f//499k954d6OL34oL9FSTvy64sA"));
    let key = long_term_key("\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}", "example.org", "TheMatrIX");
    assert!(message.verify_integrity(&key, hmac()));
    assert_eq!(long_term_key("user", "realm", "pass"), hex("8493fbc53ba582fb4c044c456bdc40eb").as_slice());
}

#[test]
fn encoder_reproduces_the_rfc_vectors() {
    let original = hex(LONG_TERM_REQUEST);
    let tid: TransactionId = original[8..20].try_into().unwrap();
    let key = long_term_key("\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}", "example.org", "TheMatrIX");
    let encoded = Encoder::new(Class::Request, method::BINDING, tid)
        .attr(attr::USERNAME, "\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}".as_bytes())
        .attr(attr::NONCE, b"f//499k954d6OL34oL9FSTvy64sA")
        .attr(attr::REALM, b"example.org")
        .finish(Some((&key, hmac())), false);
    assert_eq!(encoded, original);
    let response = hex(SAMPLE_RESPONSE);
    let tid: TransactionId = response[8..20].try_into().unwrap();
    let encoded = Encoder::new(Class::Success, method::BINDING, tid)
        .attr(attr::SOFTWARE, b"test vector")
        .xor_address(attr::XOR_MAPPED_ADDRESS, "192.0.2.1:32853".parse().unwrap())
        .finish(Some((b"VOkJxbRl1RmTxUk/WvJxBt", hmac())), true);
    // The RFC pads its SOFTWARE with a space, the encoder with zero.
    assert_eq!(encoded.len(), response.len());
    let parsed = Message::parse(&encoded).unwrap();
    assert!(parsed.fingerprint_ok() && parsed.verify_integrity(b"VOkJxbRl1RmTxUk/WvJxBt", hmac()));
}

#[test]
fn message_types_round_trip() {
    for class in [Class::Request, Class::Indication, Class::Success, Class::Error] {
        for method in [method::BINDING, method::ALLOCATE, method::CHANNEL_BIND, 0xFFF] {
            assert_eq!(split_type(message_type(class, method)), (class, method));
        }
    }
    assert_eq!(message_type(Class::Success, method::ALLOCATE), 0x0103);
    assert_eq!(message_type(Class::Error, method::ALLOCATE), 0x0113);
    assert_eq!(message_type(Class::Indication, method::SEND), 0x0016);
    assert_eq!(message_type(Class::Indication, method::DATA), 0x0017);
}

#[test]
fn ipv6_addresses_round_trip() {
    let tid = [7u8; 12];
    let addr: SocketAddr = "[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap();
    let data = Encoder::new(Class::Success, method::ALLOCATE, tid)
        .xor_address(attr::XOR_RELAYED_ADDRESS, addr)
        .finish(None, false);
    assert_eq!(Message::parse(&data).unwrap().xor_address(attr::XOR_RELAYED_ADDRESS), Some(addr));
}

#[test]
fn malformed_messages_are_refused() {
    let good = hex(SAMPLE_REQUEST);
    assert_eq!(Message::parse(&good[..19]).unwrap_err(), StunError::Short);
    let mut bad_cookie = good.clone();
    bad_cookie[4] ^= 1;
    assert_eq!(Message::parse(&bad_cookie).unwrap_err(), StunError::NotStun);
    let mut long = good.clone();
    long.extend_from_slice(&[0, 0, 0, 0]);
    assert!(matches!(Message::parse(&long), Err(StunError::Length { .. })));
    // An attribute whose length runs past the end.
    let mut overrun = good.clone();
    overrun[22..24].copy_from_slice(&0x0400u16.to_be_bytes());
    assert_eq!(Message::parse(&overrun).unwrap_err(), StunError::Truncated);
    // Something after FINGERPRINT.
    let mut after = Encoder::new(Class::Request, method::BINDING, [1; 12]).finish(None, true);
    after.extend_from_slice(&[0x80, 0x22, 0, 0]);
    let length = (after.len() - HEADER) as u16;
    after[2..4].copy_from_slice(&length.to_be_bytes());
    assert_eq!(Message::parse(&after).unwrap_err(), StunError::AfterFingerprint);
    // A wrong fingerprint is seen.
    let mut wrong = good.clone();
    let last = wrong.len() - 1;
    wrong[last] ^= 1;
    assert!(!Message::parse(&wrong).unwrap().fingerprint_ok());
}

#[test]
fn attribute_count_is_bounded() {
    let mut encoder = Encoder::new(Class::Indication, method::DATA, [2; 12]);
    for _ in 0..MAX_ATTRIBUTES {
        encoder = encoder.attr(attr::SOFTWARE, b"x");
    }
    assert!(Message::parse(&encoder.finish(None, false)).is_ok());
    let mut encoder = Encoder::new(Class::Indication, method::DATA, [2; 12]);
    for _ in 0..=MAX_ATTRIBUTES {
        encoder = encoder.attr(attr::SOFTWARE, b"x");
    }
    assert_eq!(Message::parse(&encoder.finish(None, false)).unwrap_err(), StunError::TooManyAttributes);
}

#[test]
fn attributes_after_integrity_are_not_believed() {
    let key = b"k";
    let mut data = Encoder::new(Class::Success, method::ALLOCATE, [3; 12])
        .u32(attr::LIFETIME, 600)
        .finish(Some((key, hmac())), false);
    // Appended by someone without the key: ignored, and the integrity holds.
    data.extend_from_slice(&[0x00, 0x0D, 0x00, 0x04, 0, 0, 0, 1]);
    let length = (data.len() - HEADER) as u16;
    data[2..4].copy_from_slice(&length.to_be_bytes());
    let message = Message::parse(&data).unwrap();
    assert_eq!(message.u32(attr::LIFETIME), Some(600));
    assert!(message.verify_integrity(key, hmac()));
}

#[test]
fn error_codes_and_channel_numbers() {
    let data = Encoder::new(Class::Error, method::ALLOCATE, [4; 12])
        .error(438, "Stale Nonce")
        .channel(0x4001)
        .finish(None, false);
    let message = Message::parse(&data).unwrap();
    assert_eq!(message.error_code(), Some((438, "Stale Nonce")));
    assert_eq!(message.channel_number(), Some(0x4001));
    let bogus =
        Encoder::new(Class::Error, method::ALLOCATE, [4; 12]).attr(attr::ERROR_CODE, &[0, 0, 7, 0]).finish(None, false);
    assert_eq!(Message::parse(&bogus).unwrap().error_code(), None);
}

#[test]
fn channel_data_framing() {
    let datagram = channel_data(0x4000, b"hello").unwrap();
    assert_eq!(ChannelData::parse(&datagram), Some(ChannelData { channel: 0x4000, data: b"hello" }));
    let mut padded = datagram.clone();
    padded.extend_from_slice(&[0, 0, 0]);
    assert!(ChannelData::parse(&padded).is_some());
    padded.push(0);
    assert!(ChannelData::parse(&padded).is_none(), "more than the padding");
    assert!(ChannelData::parse(&datagram[..6]).is_none(), "shorter than its length");
    assert!(ChannelData::parse(&channel_data(0x3FFF, b"x").unwrap()).is_none());
    assert!(ChannelData::parse(&channel_data(0x5000, b"x").unwrap()).is_none());
}

#[test]
fn stun_is_told_from_media() {
    assert!(looks_like_stun(&hex(SAMPLE_REQUEST)));
    assert!(!looks_like_stun(&channel_data(0x4000, b"12345678").unwrap()));
    assert!(!looks_like_stun(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0]));
    assert!(!looks_like_stun(&[0, 1]));
}

#[test]
fn a_binding_request_names_the_fragment_it_is_addressed_to() {
    // RFC 5769's request is a binding request from "h6vY" to "evtj".
    assert_eq!(ice_ufrag(&hex(SAMPLE_REQUEST)), Some("evtj"));
    let request = |class, method, username: &[u8]| {
        Encoder::new(class, method, [5; 12]).attr(attr::USERNAME, username).finish(None, true)
    };
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, b"server:client")), Some("server"));
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, b"a:b:c")), Some("a"));
    // Not a binding request, no fragment, no colon, not text.
    assert_eq!(ice_ufrag(&request(Class::Success, method::BINDING, b"server:client")), None);
    assert_eq!(ice_ufrag(&request(Class::Indication, method::BINDING, b"server:client")), None);
    assert_eq!(ice_ufrag(&request(Class::Request, method::ALLOCATE, b"server:client")), None);
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, b":client")), None);
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, b"server")), None);
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, &[0xff, b':', b'x'])), None);
    assert_eq!(ice_ufrag(&request(Class::Request, method::BINDING, &[b'a'; 600])), None, "longer than RFC 8489 allows");
    assert_eq!(ice_ufrag(&Encoder::new(Class::Request, method::BINDING, [5; 12]).finish(None, true)), None);
    // Media and truncated messages.
    assert_eq!(ice_ufrag(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0]), None);
    assert_eq!(ice_ufrag(&hex(SAMPLE_REQUEST)[..30]), None);
}
