//! Which FFmpeg runs the transcodes, as far as the relay's input flags care
//! (W4, D36).
//!
//! A relay input is read without FFmpeg's 20-picture frame-rate guess
//! (`-fpsprobesize 0`), and a slow webcam's rate is said in the relay's
//! `a=framerate` instead. FFmpeg reads that attribute since 8.0 only: 6.1
//! and 7.1 leave the rate unknown (measured 2026-11-19, `debian:trixie`'s
//! 7.1.5 on an SDP that says `a=framerate:2`: `avg_frame_rate=0/0`), and a
//! one-picture-a-second webcam would then be encoded as if it sent 90000.
//! So the guess is skipped only for an FFmpeg that reads the rate it is
//! told, asked once (`ffmpeg -version`) when the first transcode starts.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use crate::process::{Launch, command, spawn};

/// The first release that reads `a=framerate` from an RTSP description.
const READS_FRAMERATE_FROM: u32 = 8;

/// How long `ffmpeg -version` may take.
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The release line of `ffmpeg -version`'s first line (`ffmpeg version
/// 8.0.1-full_build-www.gyan.dev`, `7.1.5-0+deb13u1`, `n8.0`): its major
/// number. A build from FFmpeg's development branch (`N-121000-g...`) is
/// newer than every release.
pub(crate) fn major(first_line: &str) -> Option<u32> {
    let version = first_line.trim().strip_prefix("ffmpeg version ")?.split_whitespace().next()?;
    if version.starts_with("N-") {
        return Some(u32::MAX);
    }
    let digits: String = version.trim_start_matches('n').chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Whether FFmpeg reads the rate the relay says (`a=framerate`).
pub(crate) fn reads_framerate(major: Option<u32>) -> bool {
    major.is_some_and(|m| m >= READS_FRAMERATE_FROM)
}

/// The answer, asked once per factory.
#[derive(Debug, Default)]
pub(crate) struct Version(OnceCell<Option<u32>>);

impl Version {
    /// FFmpeg's major release, `None` when it could not be read (then the
    /// guess is kept, as before).
    pub(crate) async fn major(&self, launch: &Launch) -> Option<u32> {
        *self.0.get_or_init(|| ask(launch)).await
    }
}

async fn ask(launch: &Launch) -> Option<u32> {
    let args = ["-version".to_owned()];
    let mut process = spawn(command(launch, &args, Stdio::null(), Stdio::piped()), &launch.bin).await.ok()?;
    let mut stdout = process.child.stdout.take()?;
    let mut text = Vec::new();
    let read = tokio::time::timeout(ASK_TIMEOUT, async {
        let mut chunk = [0u8; 512];
        // The first line is all that is needed; stop once it is there.
        while !text.contains(&b'\n') && text.len() < 4096 {
            let n = stdout.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            text.extend_from_slice(chunk.get(..n)?);
        }
        Some(())
    })
    .await;
    process.kill();
    read.ok()??;
    let line = String::from_utf8_lossy(&text);
    let found = major(line.lines().next().unwrap_or_default());
    tracing::info!(major = ?found, reads_framerate = reads_framerate(found), "ffmpeg release");
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_lines_ffmpeg_prints_are_read() {
        let lines = [
            ("ffmpeg version 8.0.1-full_build-www.gyan.dev Copyright (c) 2000-2025 the FFmpeg developers", Some(8)),
            ("ffmpeg version 7.1.5-0+deb13u1 Copyright (c) 2000-2026 the FFmpeg developers", Some(7)),
            ("ffmpeg version 6.1.1-3ubuntu5 Copyright (c) 2000-2023 the FFmpeg developers", Some(6)),
            ("ffmpeg version n8.0 Copyright (c) 2000-2025 the FFmpeg developers", Some(8)),
            ("ffmpeg version N-121000-g0123456789 Copyright (c) 2000-2026", Some(u32::MAX)),
            ("ffmpeg version git-2026-01-01 Copyright", None),
            ("not ffmpeg at all", None),
            ("", None),
        ];
        for (line, expected) in lines {
            assert_eq!(major(line), expected, "{line}");
        }
        assert!(reads_framerate(Some(8)) && reads_framerate(Some(u32::MAX)));
        assert!(!reads_framerate(Some(7)) && !reads_framerate(None));
    }

    #[tokio::test]
    async fn the_installed_ffmpeg_says_its_release() {
        let default = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
        let bin = std::path::PathBuf::from(std::env::var("OXRTC_FFMPEG").unwrap_or_else(|_| default.to_owned()));
        if !bin.is_file() {
            crate::skip(&format!("{} not found (set OXRTC_FFMPEG)", bin.display()));
            return;
        }
        let config = crate::FfmpegConfig { bin, ..crate::FfmpegConfig::default() };
        let version = Version::default();
        let found = version.major(&Launch::new(&config)).await;
        assert!(found.is_some_and(|m| (6..100).contains(&m) || m == u32::MAX), "{found:?}");
    }
}
