//! A configuration file's text: `POST /api/config`'s body, and the file
//! `PUT` and `DELETE /api/streams` edit (D48).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_api::fuzzing::config_text(data) {
        panic!("{broken}");
    }
});
