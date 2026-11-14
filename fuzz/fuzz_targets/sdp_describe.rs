//! `DESCRIBE`'s SDP for any track list, read back by `ANNOUNCE`'s parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxrtc_rtsp_server::fuzzing::{sdp_roundtrip, tracks_from_bytes};

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = sdp_roundtrip(&tracks_from_bytes(data)) {
        panic!("{broken}");
    }
});
