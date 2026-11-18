//! The external binaries the interoperability tests run - FFmpeg and ffprobe,
//! skipped (with a SKIP line) when absent - and what is read from them.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use oxrtc_core::Codec;

pub const FFMPEG_DEFAULT: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
pub const FFPROBE_DEFAULT: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffprobe.exe";

/// Says why a test skips itself (a program it needs is not on this machine),
/// and fails it instead where `OXRTC_NO_SKIP` is set, as CI sets it: a skipped
/// test passes, and `cargo test` shows nothing a passing test printed, so a
/// wrong path would turn a job green without running anything.
pub fn skip(why: &str) {
    assert!(std::env::var_os("OXRTC_NO_SKIP").is_none(), "{why}: a skip is a failure where OXRTC_NO_SKIP is set");
    eprintln!("SKIP: {why}");
}

fn binary(var: &str, default: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_owned()));
    if path.is_file() {
        Some(path)
    } else {
        skip(&format!("{} not found (set {var})", path.display()));
        None
    }
}

pub fn ffmpeg() -> Option<PathBuf> {
    binary("OXRTC_FFMPEG", FFMPEG_DEFAULT)
}

pub fn ffprobe() -> Option<PathBuf> {
    binary("OXRTC_FFPROBE", FFPROBE_DEFAULT)
}

/// Runs a binary with a deadline, returning (success, stdout, stderr).
pub async fn run(program: &std::path::Path, args: &[&str], deadline: Duration) -> (bool, String, String) {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = tokio::time::timeout(deadline, child.wait_with_output()).await.expect("binary timed out").unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Encodes `seconds` of test pattern to an Annex B file with access unit
/// delimiters, and returns its bytes.
pub async fn encode(ffmpeg: &std::path::Path, codec: Codec, seconds: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let (encoder, bsf, format, extra): (&str, &str, &str, &[&str]) = match codec {
        Codec::H264 => ("libx264", "h264_metadata=aud=insert", "h264", &["-bf", "0"]),
        _ => ("libx265", "hevc_metadata=aud=insert", "hevc", &["-x265-params", "log-level=error:bframes=0"]),
    };
    let out = dir.path().join(format!("clip.{format}"));
    let out_s = out.to_str().unwrap().to_owned();
    let secs = seconds.to_string();
    let mut args = vec![
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=25",
        "-t",
        &secs,
        "-c:v",
        encoder,
        "-g",
        "25",
        "-pix_fmt",
        "yuv420p",
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["-bsf:v", bsf, "-f", format, "-y", &out_s]);
    let (ok, _, err) = run(ffmpeg, &args, Duration::from_secs(60)).await;
    assert!(ok, "encoding failed: {err}");
    tokio::fs::read(&out).await.unwrap()
}

/// Reads `url` for `seconds` with `-c copy` (over `transport`, or FFmpeg's
/// default) and returns how many frames FFmpeg wrote, with its error output.
///
/// Counted from the `framecrc` muxer, one line per packet written, and not
/// from the `frame=` of `-progress`: FFmpeg 6.1 prints that key for an
/// encoded picture only, so a copy reads as no frame at all there (7.1 and 8.0
/// count the copied packets too).
pub async fn copied_frames(url: &str, transport: Option<&str>, seconds: u32) -> (u64, String) {
    let ffmpeg = ffmpeg().unwrap();
    let seconds = seconds.to_string();
    let mut args = vec!["-hide_banner", "-nostats", "-loglevel", "error"];
    if let Some(transport) = transport {
        args.extend_from_slice(&["-rtsp_transport", transport]);
    }
    args.extend_from_slice(&["-i", url, "-t", &seconds, "-c", "copy", "-f", "framecrc", "-"]);
    let (ok, out, err) = run(&ffmpeg, &args, Duration::from_secs(30)).await;
    assert!(ok, "ffmpeg failed: {err}");
    // A count of zero must mean "no frame", never "another output format".
    assert!(out.contains("#tb 0:"), "not framecrc's output: {out:?} (stderr: {err})");
    (framecrc_packets(&out), err)
}

/// The packets in a `framecrc` output: every line but the `#` header's.
pub fn framecrc_packets(output: &str) -> u64 {
    output.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count() as u64
}
