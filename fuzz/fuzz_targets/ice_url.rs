//! A `webrtc.ice_servers` URL, as the configuration file names it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_webrtc::fuzzing::ice_url(data) {
        panic!("{broken}");
    }
});
