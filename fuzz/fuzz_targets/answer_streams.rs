//! An SDP answer through the rewrite of its stream ids: the text str0m
//! serialised from an offer a remote browser wrote, read line by line before
//! `POST /api/webrtc` returns it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(broken) = oxrtc_webrtc::fuzzing::answer_streams(data) {
        panic!("{broken}");
    }
});
