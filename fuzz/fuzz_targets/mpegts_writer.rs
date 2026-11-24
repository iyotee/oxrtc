//! Tracks (H.264, H.265, AAC with a configuration from the input, Opus) and
//! frames of every shape through the MPEG-TS writer, read back by the
//! reader: whole packets, counters unbroken, each accepted frame one PES at
//! its time with its bytes, a refused frame writing nothing.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_mpegts::fuzzing::writer_bytes(data) {
        panic!("{broken}");
    }
});
