//! A WHEP offer, the one SDP a remote browser writes entirely: read by the
//! same code `POST /api/webrtc` runs before any session exists.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = oxrtc_webrtc::OfferedCodecs::from_sdp(text);
    }
});
