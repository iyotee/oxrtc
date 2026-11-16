//! An RTSP `Authorization: Digest` parameter list through the reader the
//! server runs on a reader's header.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::authorization(data) {
        panic!("{broken}");
    }
});
