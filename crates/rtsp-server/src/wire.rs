//! What the server writes on an RTSP connection: responses and interleaved
//! frame headers. Reading is [`crate::framing`]'s.

use bytes::{Bytes, BytesMut};
use rtsp_types::headers::{CSEQ, SERVER};
use rtsp_types::{Request, Response, StatusCode, Version};

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
    Bytes::from(split_challenges(out))
}

/// What the `401` of `auth.rs` puts in one `WWW-Authenticate` value (the header
/// map holds one value per name and joins more with a comma): Digest, then
/// `Basic`. A client reads one scheme per line, and a Basic-only player does not
/// look for its scheme behind a Digest one, so the two go out as two lines.
pub(crate) const CHALLENGE_JOIN: &str = ", Basic realm=";

fn split_challenges(out: Vec<u8>) -> Vec<u8> {
    const EOL: &[u8] = &[13, 10];
    let marker = CHALLENGE_JOIN.as_bytes();
    let Some(at) = out.windows(marker.len()).position(|w| w == marker) else { return out };
    let line_start = out.get(..at).and_then(|head| head.windows(2).rposition(|w| w == EOL)).map_or(0, |i| i + 2);
    let name = out
        .get(line_start..at)
        .and_then(|line| line.iter().position(|&b| b == b':').and_then(|colon| line.get(..colon)));
    let Some(name) = name.filter(|name| name.eq_ignore_ascii_case(b"www-authenticate")) else { return out };
    let mut split = Vec::with_capacity(out.len() + 24);
    split.extend_from_slice(out.get(..at).unwrap_or_default());
    split.extend_from_slice(EOL);
    split.extend_from_slice(name);
    split.extend_from_slice(b": Basic realm=");
    split.extend_from_slice(out.get(at + marker.len()..).unwrap_or_default());
    split
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
    use rtsp_types::headers::{CSEQ, PUBLIC, WWW_AUTHENTICATE};

    use super::*;

    fn text(response: &Response<Vec<u8>>) -> String {
        String::from_utf8(encode(response).to_vec()).unwrap()
    }

    #[test]
    fn two_challenges_go_out_as_two_lines() {
        let response = Response::builder(Version::V1_0, StatusCode::Unauthorized)
            .header(CSEQ, "3")
            .header(WWW_AUTHENTICATE, "Digest realm=\"oxrtc\", nonce=\"ab\", qop=\"auth\"")
            .header(WWW_AUTHENTICATE, "Basic realm=\"oxrtc\"")
            .build(Vec::new());
        let lines: Vec<String> = text(&response).lines().map(str::to_owned).collect();
        let challenges: Vec<&String> =
            lines.iter().filter(|l| l.to_ascii_lowercase().starts_with("www-authenticate")).collect();
        assert_eq!(challenges.len(), 2, "{lines:?}");
        assert!(challenges[0].ends_with("Digest realm=\"oxrtc\", nonce=\"ab\", qop=\"auth\""), "{challenges:?}");
        assert!(challenges[1].ends_with(": Basic realm=\"oxrtc\""), "{challenges:?}");
        assert!(lines.iter().any(|l| l.starts_with("CSeq")), "the other headers are kept");
    }

    #[test]
    fn nothing_else_is_rewritten() {
        let response =
            Response::builder(Version::V1_0, StatusCode::Ok).header(PUBLIC, "OPTIONS, Basic realm=x").build(Vec::new());
        assert!(text(&response).contains("OPTIONS, Basic realm=x"), "only the challenge header is split");
    }
}
