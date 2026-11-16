//! The body of a `PATCH` on a WHEP session resource: a trickle ICE fragment.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_webrtc::fuzzing::sdp_fragment(data) {
        panic!("{broken}");
    }
});
