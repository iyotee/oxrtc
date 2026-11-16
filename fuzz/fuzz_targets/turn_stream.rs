//! What a TURN server over TCP - or anyone who can send on that connection -
//! makes the TURN client read: a byte stream with no length in front of its
//! messages, cut at the reads' boundaries and into messages the allocation
//! takes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_webrtc::fuzzing::turn_stream(data) {
        panic!("{broken}");
    }
});
