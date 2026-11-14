//! An `ANNOUNCE` body, as a publisher sends it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::sdp_announce(data) {
        panic!("{broken}");
    }
});
