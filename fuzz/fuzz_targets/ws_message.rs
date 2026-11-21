//! A text frame of `/api/ws`, go2rtc's WebSocket API.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_api::fuzzing::ws_message(data) {
        panic!("{broken}");
    }
});
