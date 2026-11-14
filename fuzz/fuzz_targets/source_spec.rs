//! A source spec (the configuration and `PUT /api/streams` carry them) and a
//! consumer query (`?video=h264&audio`, from `DESCRIBE` URLs and the API).
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxrtc_core::{SourceSpec, Want};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(spec) = SourceSpec::parse(text) {
        let _ = (spec.scheme(), spec.base(), spec.fragments().count(), spec.redacted());
    }
    let _ = Want::from_query(text);
});
