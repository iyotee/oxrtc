//! What a TURN server - or anyone who can send from its address - makes the
//! TURN client parse: datagrams, answers to its requests, data and time,
//! through the same allocation the relay driver runs.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_webrtc::fuzzing::turn_datagrams(data) {
        panic!("{broken}");
    }
});
