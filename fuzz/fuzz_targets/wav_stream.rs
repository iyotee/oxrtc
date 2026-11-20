//! A WAV stream as an HTTP server sends it (openIVR's sound de-duplicator),
//! cut at the chosen read size and in one piece: the bounds, the same format
//! and the same samples.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_audio::fuzzing::wav_bytes(data) {
        panic!("{broken}");
    }
});
