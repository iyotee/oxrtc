//! A JPEG frame (a whole file, or a scan behind valid headers) through the
//! RTP/JPEG reader, fragments and packetizer, and back through the
//! depacketizer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_rtsp_server::fuzzing::jpeg_frame(data) {
        panic!("{broken}");
    }
});
