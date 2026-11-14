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
