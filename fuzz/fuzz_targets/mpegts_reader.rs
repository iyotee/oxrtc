//! Any bytes into the MPEG-TS reader, as they are and with each packet's
//! sync byte set: a value or an error, never more payload than input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_mpegts::fuzzing::reader_bytes(data) {
        panic!("{broken}");
    }
});
