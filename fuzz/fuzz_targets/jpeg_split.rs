//! A byte stream of JPEG pictures as FFmpeg writes it to a pipe (`-f mjpeg`,
//! the Motion JPEG transcode of `/api/stream.mjpeg`), cut at a chosen read
//! size under a chosen limit: each picture from a start to an end of image,
//! within the limit, in the input in order; the bytes held bounded; the same
//! pictures whatever the reads. The first byte is the read size less one,
//! the second the limit in KiB less one; its proptest twins are
//! `oxrtc-core`'s `any_bytes_keep_the_invariants` and
//! `streams_of_pictures_keep_the_invariants`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [read, kib, rest @ ..] = data else { return };
    let read = usize::from(*read) + 1;
    let max = (usize::from(*kib) + 1) * 1024;
    if let Err(broken) = oxrtc_core::jpeg::split_invariants(rest, read, max) {
        panic!("{broken}");
    }
});
