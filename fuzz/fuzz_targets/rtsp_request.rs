//! The RTSP connection's framing and request handling, from raw bytes: the
//! first three bytes choose the request limit and the read size, the rest is
//! what a peer sends. See `oxrtc_rtsp_server::fuzzing::rtsp_stream`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::rtsp_stream_bytes(data) {
        panic!("{broken}");
    }
});
