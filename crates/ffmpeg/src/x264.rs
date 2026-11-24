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

/// `-threads:<stream> 2` for each output stream the arguments code with
/// x264, when neither their output side nor the output template say a thread
/// count; nothing otherwise. The operator's own count always wins.
///
/// Only the output side is read: a `-threads` before the last `-i` is the
/// decoder's count (FFmpeg reads an option before `-i` as the input's), and
/// left x264 at its default; and only the streams x264 codes are given the
/// count, so another encoder of the same run (`#video=h264#video=h265`) keeps
/// FFmpeg's default.
pub(crate) fn threads_unless_said(args: &[String], output: &[String]) -> Vec<String> {
    let output_side: Vec<&String> = args.iter().skip(output_side_start(args)).chain(output).collect();
    if names_threads(&output_side) {
        return Vec::new();
    }
    let mut threads = Vec::new();
    for pair in output_side.windows(2) {
        let [option, value] = pair else { continue };
        if value.as_str() != "libx264" {
            continue;
        }
        let Some(stream) = codec_stream(option) else { continue };
        let option = format!("-threads{stream}");
        if !threads.contains(&option) {
            threads.push(option);
        }
    }
    threads.into_iter().flat_map(|option| [option, THREADS.to_owned()]).collect()
}

/// Where the output's options start: after the last input's URL.
fn output_side_start(args: &[String]) -> usize {
    args.iter().rposition(|arg| arg == "-i").map_or(0, |at| at.saturating_add(2))
}

/// The stream specifier a codec option names, with its colon (`-c:v:0` gives
/// `:v:0`; `-vcodec`, `-c` and `-codec` give `:v`, the video streams);
/// `None` for any other option.
fn codec_stream(option: &str) -> Option<String> {
    if option == "-vcodec" {
        return Some(":v".to_owned());
    }
    let rest = option.strip_prefix("-codec").or_else(|| option.strip_prefix("-c"))?;
    match rest {
        "" => Some(":v".to_owned()),
        stream if stream.starts_with(':') => Some(stream.to_owned()),
        _ => None,
    }
}

/// Whether a thread count is already said: FFmpeg's `-threads` (any stream
/// specifier) or x264's own (`threads=`, `sliced-threads=` in the value of
/// `-x264-params` or `-x264opts`, any stream specifier).
fn names_threads(args: &[&String]) -> bool {
    let ffmpeg = args.iter().any(|arg| arg.as_str() == "-threads" || arg.starts_with("-threads:"));
    let x264 = args.windows(2).any(|pair| {
        let [option, value] = pair else { return false };
        let named = ["-x264-params", "-x264opts"]
            .iter()
            .any(|name| option.as_str() == *name || option.strip_prefix(name).is_some_and(|s| s.starts_with(':')));
        named
            && value.split(':').any(|item| {
                let key = item.split('=').next().unwrap_or_default();
                key == "threads" || key == "sliced-threads"
            })
    });
    ffmpeg || x264
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn an_x264_encode_with_no_count_gets_two_threads() {
        for line in ["-c:v libx264 -g 50", "-codec:v libx264 -g:v 30", "-vcodec libx264"] {
            assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v", "2"], "{line}");
        }
        assert_eq!(threads_unless_said(&words("-map 0:v:0? -c:v:0 libx264"), &[]), ["-threads:v:0", "2"]);
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
    /// A `-threads` before `-i` is the decoder's: x264's count is still
    /// unsaid (it left x264 at its default before, 71 threads of FFmpeg 8.0.1
    /// against 29 on a 1080p encode, review of 2026-11-24).
    #[test]
    fn a_decoder_count_is_not_the_encoders() {
        let line = "-threads 1 -flags low_delay -i rtsp://127.0.0.1:8554/camera_1_src -codec:v libx264 -g:v 30";
        assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v", "2"]);
        // The same count after the input is the encoder's.
        let line = "-i rtsp://127.0.0.1:8554/camera_1_src -threads 1 -codec:v libx264";
        assert!(threads_unless_said(&words(line), &[]).is_empty());
    }

    /// Only an x264 option says x264's count: a URL or a filter that holds
    /// `threads=` does not, nor x264's lookahead threads.
    #[test]
    fn threads_said_anywhere_else_is_not_a_count() {
        for line in [
            "-i http://cam.example/video?threads=4 -c:v libx264",
            "-i rtsp://x -c:v libx264 -x264-params lookahead-threads=2:keyint=50",
            "-i rtsp://x -c:v libx264 -metadata comment=threads=3",
        ] {
            assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v", "2"], "{line}");
        }
        for line in [
            "-i rtsp://x -c:v libx264 -x264-params keyint=50:threads=3",
            "-i rtsp://x -c:v libx264 -x264-params:v sliced-threads=0",
            "-i rtsp://x -c:v libx264 -x264opts threads=1",
        ] {
            assert!(threads_unless_said(&words(line), &[]).is_empty(), "{line}");
        }
    }

    /// go2rtc's multi-codec mode: only the stream x264 codes gets the count.
    #[test]
    fn only_the_streams_x264_codes_are_given_the_count() {
        let line = "-i rtsp://x -map 0:v:0? -c:v:0 libx264 -g 50 -map 0:v:0? -c:v:1 libx265 -map 0:a:0? -c:a:0 libopus";
        assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v:0", "2"]);
        let line = "-i rtsp://x -map 0:v:0? -c:v:0 libx264 -map 0:v:0? -c:v:1 libx264";
        assert_eq!(threads_unless_said(&words(line), &[]), ["-threads:v:0", "2", "-threads:v:1", "2"]);
        assert_eq!(threads_unless_said(&words("-i rtsp://x -c libx264"), &[]), ["-threads:v", "2"]);
    }
}
