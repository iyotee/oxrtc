//! The query of `/api/stream.mp4` and `/api/frame.mp4`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_api::fuzzing::mp4_query(data) {
        panic!("{broken}");
    }
});
