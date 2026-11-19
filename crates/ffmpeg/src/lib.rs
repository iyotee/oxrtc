//! FFmpeg for oxrtc, always as a child process, confined on Linux (D5, D6).
//!
//! - [`FfmpegFactory`] runs `ffmpeg:` sources in go2rtc's notation
//!   (`ffmpeg:camera_1_src#video=h264#width=1920#height=1080`,
//!   `ffmpeg:rtsp://...#audio=opus#timeout=20`,
//!   `ffmpeg:http://...#audio=opus#input=dedup_input`). The arguments are
//!   built as go2rtc 1.9.15 builds them, from the same default templates
//!   overridden by the configuration's `ffmpeg:` section; a stream name is read
//!   through oxrtc's own RTSP server, and the output is published back into it
//!   on a one-time URL that a [`PublishTarget`] hands out.
//! - [`Stills`] turns a stream's last keyframe into a JPEG.
//!
//! FFmpeg is started from an argument vector (never a shell), with a cleared
//! environment, no console window, `kill_on_drop`, and its error output drained
//! into the log at debug level, redacted. It dies with oxrtc, crash included:
//! on Windows through a job object, on Linux through `oxrtc-exec`
//! ([`Confinement`]), which also limits it and restricts its file system. A source may name a template, never
//! supply one: text from a source never becomes an FFmpeg argument, except
//! the input URL (as one argument) and checked numbers.
//!
//! This crate depends on the core only; the binary implements
//! [`PublishTarget`] with the RTSP server's publication tokens.
#![forbid(unsafe_code)]

mod args;
mod confine;
#[cfg(windows)]
mod console;
mod driver;
mod limiter;
mod process;
mod relay;
mod spawner;
pub mod split;
mod stills;
mod templates;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use oxrtc_core::ProducerSink;

pub use confine::{Confinement, Discovery, FFMPEG_LIMITS, Landlock, Limits};
pub use driver::FfmpegFactory;
pub use stills::{MAX_WIDTH, StillError, Stills};

/// Settings shared by the transcoders and the stills.
#[derive(Clone)]
pub struct FfmpegConfig {
    /// The FFmpeg executable.
    pub bin: PathBuf,
    /// The configuration file's `ffmpeg:` section (every key but `bin`): named
    /// templates overriding the built-in defaults (`h264`, `rtsp`, `global`...)
    /// or adding new ones (`dedup_input`).
    pub templates: BTreeMap<String, String>,
    /// `127.0.0.1:<port>` of oxrtc's own RTSP server, where a source that
    /// names a stream reads it.
    pub relay: String,
    /// Most transcoding processes running at once; a run beyond it fails (and
    /// is retried by the core). At least one is always allowed.
    pub max_processes: usize,
    /// How long a started transcoder may take to publish before it is killed.
    pub start_timeout: Duration,
    /// FFmpeg started through `oxrtc-exec` (Linux); `None` starts it directly.
    pub confine: Option<Confinement>,
}

impl Default for FfmpegConfig {
    /// `ffmpeg` from the `PATH`, go2rtc's defaults, the RTSP server on its
    /// default port, 64 processes, go2rtc's start timeout of 30 s, started
    /// directly (the binary sets [`confine`](Self::confine)).
    fn default() -> Self {
        Self {
            bin: PathBuf::from("ffmpeg"),
            templates: BTreeMap::new(),
            relay: "127.0.0.1:8554".into(),
            // openIVR runs two per watched camera (its sound transcode, always
            // on, and the H.264 one): 16 starved cameras 9 and up of both in
            // the bench's B07 (10 cameras), silently, in a retry loop. 64 is
            // 32 such cameras; go2rtc has no cap at all.
            max_processes: 64,
            start_timeout: Duration::from_secs(30),
            confine: None,
        }
    }
}

impl std::fmt::Debug for FfmpegConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // An operator may write a fixed input URL, credentials included.
        let templates: BTreeMap<&str, String> =
            self.templates.iter().map(|(name, text)| (name.as_str(), oxrtc_core::spec::redact(text))).collect();
        f.debug_struct("FfmpegConfig")
            .field("bin", &self.bin)
            .field("templates", &templates)
            .field("relay", &self.relay)
            .field("max_processes", &self.max_processes)
            .field("start_timeout", &self.start_timeout)
            .field("confine", &self.confine)
            .finish()
    }
}

/// Where a transcoder publishes: the binary implements it with the RTSP
/// server's `Publishers`.
pub trait PublishTarget: Send + Sync + 'static {
    /// Registers `sink` and returns the lease of a one-time publication URL;
    /// media published there goes into `sink` until the lease is dropped.
    fn lease(&self, sink: ProducerSink) -> Box<dyn PublishLease>;
}

/// A registered publication URL (the RTSP server's `PublishHandle`); dropping
/// it ends the publication.
pub trait PublishLease: Send + Sync {
    /// `rtsp://127.0.0.1:<port>/<one-time path>`: FFmpeg's `{output}`.
    fn url(&self) -> String;

    /// The user name and password this run reads the server's streams with,
    /// when the server's readers must authenticate (`rtsp.username` is set):
    /// put in the input URL of an `ffmpeg:` source that reads the server, so
    /// the process carries a credential of its own, which ends with the lease,
    /// and never the operator's (D24). `None` when no reader needs one.
    fn read_credential(&self) -> Option<(String, String)> {
        None
    }
}

/// Says why a test skips itself (a program it needs is not on this machine),
/// and fails it instead where `OXRTC_NO_SKIP` is set, as CI sets it: a skipped
/// test passes, and `cargo test` shows nothing a passing test printed, so a
/// wrong path would turn a job green without running anything.
#[cfg(test)]
pub(crate) fn skip(why: &str) {
    assert!(std::env::var_os("OXRTC_NO_SKIP").is_none(), "{why}: a skip is a failure where OXRTC_NO_SKIP is set");
    eprintln!("SKIP: {why}");
}
