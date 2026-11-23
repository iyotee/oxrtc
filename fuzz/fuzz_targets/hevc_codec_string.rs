//! An H.265 entry of an MSE codec list (`hvc1.1.6.L153.B0`), as a page sends
//! it to `/api/ws`: read into what the reader's decoder takes (D44), or
//! refused; what is read names numbers within their fields, and the string
//! written back from them is read the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxrtc_core::h265_receiver::HevcReceiver;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let Some(read) = HevcReceiver::from_codec_string(text) else { return };
    assert!(read.space <= 3 && read.tier <= 1, "{text:?} read as {read:?}");
    assert!(read.profile.is_none_or(|p| p <= 31), "{text:?} read as {read:?}");
    if let (Some(profile), Some(level)) = (read.profile, read.level) {
        let space = ["", "A", "B", "C"][usize::from(read.space)];
        let tier = if read.tier == 1 { 'H' } else { 'L' };
        let again = format!("hvc1.{space}{profile}.0.{tier}{level}");
        assert_eq!(HevcReceiver::from_codec_string(&again), Some(read), "{again:?}");
    }
});
