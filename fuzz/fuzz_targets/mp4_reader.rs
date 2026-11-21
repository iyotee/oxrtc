//! Any bytes into the fragmented MP4 reader the writer's tests use, as an
//! init segment and as a fragment: a value or an error, every sample's bytes
//! within the input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_mp4::fuzzing::reader_bytes(data) {
        panic!("{broken}");
    }
});
