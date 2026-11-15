//! `oxrtc-exec`: how oxrtc starts FFmpeg on Linux (D5, R13, R29).
//!
//! oxrtc runs `oxrtc-exec <plan> -- ffmpeg <args>`; `oxrtc-exec` confines
//! itself, then `exec()`s FFmpeg, which keeps every restriction (it is the
//! same process):
//!
//! - **dies with oxrtc**, `SIGKILL` included: `PR_SET_PDEATHSIG(SIGKILL)`,
//!   then a check that the parent is still oxrtc (it could have died before);
//! - **no new privileges** (`PR_SET_NO_NEW_PRIVS`): no set-user-ID program,
//!   no file capability, ever raises what FFmpeg can do;
//! - **resource limits**: address space, file size, open files, no core dump,
//!   and a niceness;
//! - **Landlock**: the file system reduced to what FFmpeg reads (its binary,
//!   its libraries, name resolution, certificates), nothing written but
//!   `/dev/null`; signals and abstract UNIX sockets scoped. A kernel without
//!   Landlock runs it all the same, with a warning.
//!
//! The workspace forbids unsafe code, so this cannot be a `pre_exec` hook in
//! oxrtc: it is a program of its own, built from safe wrappers (rustix,
//! landlock). Elsewhere than Linux it refuses to run: on Windows oxrtc ties
//! FFmpeg to itself with a job object and starts it directly.
#![forbid(unsafe_code)]

mod plan;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{Report, confine};
pub use plan::{Landlock, Limits, Plan, PlanError};

/// The exit code of a refusal: a bad command line, a guarantee that could not
/// be applied, or a platform other than Linux. The program did not run.
pub const REFUSED: u8 = 125;

/// The exit code when the program was found but could not be run.
pub const NOT_RUN: u8 = 126;

/// The exit code when the program was not found.
pub const NOT_FOUND: u8 = 127;

/// The wrapper's file name, as oxrtc looks for it beside itself.
pub const FILE_NAME: &str = if cfg!(windows) { "oxrtc-exec.exe" } else { "oxrtc-exec" };
