//! The thread count oxrtc gives x264 when the operator's templates say none
//! (D49).
//!
//! Told nothing, x264 under `-tune zerolatency` (go2rtc's and openIVR's
//! `h264` templates) codes each picture in slices on one and a half threads
//! per logical CPU - 24 on a 16-thread PC - and spends a third to two fifths of
//! a transcode's CPU synchronising them. Measured on the owner's own pictures
//! (2026-11-24, M4 optimisation pass): his 2304x1296 H.265 camera through
//! openIVR's `h264/live` template took 16.9 s of CPU for 30 s by default and
//! 10.4 with two threads on Windows (16.7 and 9.6 on Linux), 60 and 35 MiB
//! less resident, the same quality setting coded in 0.8 % fewer bits (each
//! slice restarts prediction); a 704x480 webcam about a third less. go2rtc and
//! MediaMTX leave FFmpeg's default.

/// x264's threads when nothing says a count.
pub(crate) const THREADS: &str = "2";

/// `-threads:v 2` when the arguments run x264 and neither they nor the
/// output template say a thread count; nothing otherwise. The operator's own
/// count always wins.
pub(crate) fn threads_unless_said(args: &[String], output: &[String]) -> Vec<String> {
    if encodes_with_x264(args) && !names_threads(args) && !names_threads(output) {
        vec!["-threads:v".to_owned(), THREADS.to_owned()]
    } else {
        Vec::new()
    }
}

/// Whether the arguments run x264 (`-c:v libx264`, `-codec:v libx264`,
/// `-vcodec libx264`, a `#raw=` template's).
fn encodes_with_x264(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "libx264")
}

/// Whether a thread count is already said: FFmpeg's `-threads` (any stream
/// specifier) or x264's own (`threads=`, `sliced-threads=` in
/// `-x264-params` or `-x264opts`).
fn names_threads(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-threads" || arg.starts_with("-threads:") || arg.contains("threads="))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn an_x264_encode_with_no_count_gets_two_threads() {
        for line in ["-c:v libx264 -g 50", "-codec:v libx264 -g:v 30", "-vcodec libx264", "-map 0:v:0? -c:v:0 libx264"]
        {
            assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v", "2"], "{line}");
        }
    }

    #[test]
    fn a_count_the_operator_says_wins() {
        for line in [
            "-c:v libx264 -threads 4",
            "-c:v libx264 -threads:v 1",
            "-c:v libx264 -threads:v:0 3",
            "-c:v libx264 -x264-params threads=6",
            "-c:v libx264 -x264opts sliced-threads=0",
        ] {
            assert!(threads_unless_said(&words(line), &[]).is_empty(), "{line}");
        }
        // Said in the output template.
        assert!(threads_unless_said(&words("-c:v libx264"), &words("-threads 8 -f rtsp {output}")).is_empty());
    }

    #[test]
    fn no_other_encoder_is_touched() {
        for line in [
            "-c copy",
            "-c:v copy",
            "-c:v h264_nvenc",
            "-c:v libx265 -x265-params level=5.1",
            "-c:a libopus",
            "-c:v mjpeg",
        ] {
            assert!(threads_unless_said(&words(line), &[]).is_empty(), "{line}");
        }
        // Another thread option is not a thread count of the encoder's.
        assert_eq!(threads_unless_said(&words("-filter_threads 1 -c:v libx264"), &[]), ["-threads:v", "2"]);
    }
}
