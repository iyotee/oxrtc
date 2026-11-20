//! FFmpeg as the reference the codecs are checked against.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_FFMPEG: &str = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";

/// Says why a test skips itself, and fails it instead where `OXRTC_NO_SKIP`
/// is set, as CI sets it.
pub fn skip(why: &str) {
    assert!(std::env::var_os("OXRTC_NO_SKIP").is_none(), "{why}: a skip is a failure where OXRTC_NO_SKIP is set");
    eprintln!("SKIP: {why}");
}

/// `$OXRTC_FFMPEG`, else the Windows default, else `None` (and a SKIP line).
pub fn ffmpeg() -> Option<PathBuf> {
    let bin = PathBuf::from(std::env::var("OXRTC_FFMPEG").unwrap_or_else(|_| DEFAULT_FFMPEG.into()));
    if bin.is_file() {
        Some(bin)
    } else {
        skip(&format!("{} not found (set OXRTC_FFMPEG)", bin.display()));
        None
    }
}

/// What FFmpeg writes on its output for `input` on its input, with these
/// arguments between `-i -` and `-` (the input's format goes before).
pub fn run(bin: &PathBuf, input_format: &[&str], output_format: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new(bin)
        .args(["-hide_banner", "-v", "error", "-nostdin"])
        .args(input_format)
        .args(["-i", "pipe:0"])
        .args(output_format)
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Little-endian 16-bit samples.
pub fn s16le(bytes: &[u8]) -> Vec<i16> {
    bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// Samples as little-endian bytes.
pub fn to_s16le(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// The signal-to-noise ratio of `got` against `reference`, in dB, over
/// their common length.
pub fn snr_db(reference: &[i16], got: &[i16]) -> f64 {
    let (mut signal, mut noise) = (0f64, 0f64);
    for (r, g) in reference.iter().zip(got) {
        signal += f64::from(*r).powi(2);
        noise += (f64::from(*r) - f64::from(*g)).powi(2);
    }
    if noise == 0.0 { f64::INFINITY } else { 10.0 * (signal / noise).log10() }
}

/// A test signal: three tones and a sweep, at `rate`, `seconds` long, at
/// about a third of full scale.
pub fn tones(rate: u32, seconds: f64, highest: f64) -> Vec<i16> {
    let n = (f64::from(rate) * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let sweep = 200.0 + (highest - 200.0) * t / seconds;
            let v = 0.25 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 0.15 * (2.0 * std::f64::consts::PI * (highest * 0.6) * t).sin()
                + 0.1 * (2.0 * std::f64::consts::PI * sweep * t * 0.5).sin();
            (v * 32767.0) as i16
        })
        .collect()
}
