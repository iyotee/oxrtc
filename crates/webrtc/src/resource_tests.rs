#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use super::*;

const CANDIDATE: &str = "a=candidate:1 1 udp 2122260223 192.168.1.20 50000 typ host";

#[test]
fn a_resource_id_is_128_random_bits_in_hex() {
    let ids: Vec<String> = (0..64).map(|_| new_resource_id().unwrap()).collect();
    for id in &ids {
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{id}");
    }
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "no two ids alike");
}

#[test]
fn a_fragment_gives_its_user_name_fragment_and_its_candidates() {
    let body = format!(
        "a=ice-ufrag:EsAw\r\na=ice-pwd:P2uYro0UCOQ4zxjKXaWCBui1\r\nm=audio 9 RTP/AVP 0\r\na=mid:0\r\n{CANDIDATE}\r\n\
         a=candidate:2 1 udp 1686052607 198.51.100.3 50001 typ srflx raddr 192.168.1.20 rport 50000\r\n\
         a=end-of-candidates\r\n"
    );
    let fragment = parse_fragment(&body).unwrap();
    assert_eq!(fragment.ufrag.as_deref(), Some("EsAw"));
    let addrs: Vec<String> = fragment.candidates.iter().map(|c| c.addr().to_string()).collect();
    assert_eq!(addrs, ["192.168.1.20:50000", "198.51.100.3:50001"]);
    assert_eq!(fragment.unread, 0);
}

#[test]
fn a_candidate_that_cannot_be_read_is_counted_and_left_out() {
    let body =
        format!("{CANDIDATE}\na=candidate:3 1 udp 2122260223 0a1b2c3d-1234.local 50000 typ host\na=candidate:junk\n");
    let fragment = parse_fragment(&body).unwrap();
    assert_eq!((fragment.candidates.len(), fragment.unread), (1, 2));
}

#[test]
fn an_empty_or_unrelated_body_is_a_fragment_with_nothing_in_it() {
    for body in ["", "\r\n", "hello", "v=0\r\ns=-\r\n", "a=end-of-candidates\r\n"] {
        let fragment = parse_fragment(body).unwrap();
        assert!(fragment.candidates.is_empty() && fragment.ufrag.is_none(), "{body:?}");
    }
}

#[test]
fn a_patch_is_bounded_in_candidates() {
    let line = |i: usize| format!("a=candidate:{i} 1 udp 1 10.0.0.{i} 5000 typ host\r\n");
    let at_the_bound: String = (1..=MAX_CANDIDATES_PER_PATCH).map(line).collect();
    assert_eq!(parse_fragment(&at_the_bound).unwrap().candidates.len(), MAX_CANDIDATES_PER_PATCH);
    let over: String = (1..=MAX_CANDIDATES_PER_PATCH + 1).map(line).collect();
    assert!(matches!(parse_fragment(&over), Err(ResourceError::BadFragment(_))));
    // Lines that cannot be read count too: they cost a parse each.
    let unread: String = (0..=MAX_CANDIDATES_PER_PATCH).map(|_| "a=candidate:x\r\n").collect();
    assert!(matches!(parse_fragment(&unread), Err(ResourceError::BadFragment(_))));
}

#[test]
fn an_offers_user_name_fragment_is_the_first_one() {
    let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=ice-ufrag:abcd\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ice-ufrag:efgh\r\n";
    assert_eq!(offer_ufrag(sdp).as_deref(), Some("abcd"));
    assert_eq!(offer_ufrag("v=0\r\na=ice-ufrag:\r\n"), None);
    assert_eq!(offer_ufrag("v=0\r\n"), None);
}
