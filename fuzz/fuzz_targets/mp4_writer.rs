//! Tracks (with parameter sets and AAC configurations taken from the input),
//! a fragmenting and frames of every shape, through the fragmented MP4
//! writer and read back: decode times contiguous, each accepted frame's
//! bytes, offsets and flags as given, FLAC frames with their CRC.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_mp4::fuzzing::writer_bytes(data) {
        panic!("{broken}");
    }
});
