//! Frames through this server's packetizers and back through the
//! depacketizers: the first byte picks the codec, the next splits the rest
//! into NAL bodies (video) or is part of the frame (audio).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::packetize_bytes(data) {
        panic!("{broken}");
    }
});
