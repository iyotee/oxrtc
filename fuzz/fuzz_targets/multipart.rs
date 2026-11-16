//! A webcam's `multipart/x-mixed-replace` stream, cut at the chosen read
//! size and in one piece: the bounds, and the same pictures.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_http_client::fuzzing::multipart_bytes(data) {
        panic!("{broken}");
    }
});
