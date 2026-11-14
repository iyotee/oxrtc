//! RTP packets from a publisher into each depacketizer: the first byte picks
//! the codec, then 16-bit length-prefixed packets.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::depacketize_bytes(data) {
        panic!("{broken}");
    }
});
