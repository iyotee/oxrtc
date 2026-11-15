//! FFmpeg confined on Linux (D5, R13, R29): started through `oxrtc-exec`,
//! which ties it to oxrtc, limits it and restricts its file system, then
//! becomes it. This module is the policy - which paths, which limits - and the
//! command line; `oxrtc-exec` applies it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use oxrtc_exec::Plan;
pub use oxrtc_exec::{Landlock, Limits};

/// FFmpeg's limits. The address space is a ceiling for a runaway decoder, not
/// a budget: a transcode resides in about 50 MiB (D11), but its threads' stacks
/// and allocator arenas reserve far more address space than they use.
/// FFmpeg writes no file (its output is a socket or a pipe), and a niceness of
/// 2 lets oxrtc's own pass-through win a contended CPU. Not yet measured on
/// the Raspberry Pi: `ffmpeg.max_memory_mib` changes the first.
pub const FFMPEG_LIMITS: Limits =
    Limits { address_space: Some(4 << 30), file_size: Some(0), open_files: Some(256), nice: 2 };

/// What FFmpeg reads beneath, besides its own directory and
/// `LD_LIBRARY_PATH`: binaries and libraries, the loader's files, name
/// resolution, certificates (an `https:` input), time zones, the CPU count,
/// its own `/proc` entry (the wrapper's: the same process after `exec()`), the
/// usual devices. Not `/etc` as a whole (an operator may keep oxrtc's
/// configuration there, credentials included), not `/proc` (oxrtc's
/// environment), not `/home`, `/tmp`, `/run`, `/opt` or `/var`.
const READ: [&str; 32] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/nix/store",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/ld.so.preload",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
    "/etc/localtime",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/run/systemd/resolve",
    "/proc/self",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/sys/devices/system/cpu",
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/full",
];

/// The only file FFmpeg may open for writing.
const WRITE: [&str; 1] = ["/dev/null"];

/// The name services FFmpeg may ask (a camera named `cam.local`, systemd's
/// resolver, nscd).
const CONNECT: [&str; 3] = ["/run/systemd/resolve", "/run/nscd", "/run/avahi-daemon"];

/// How long the startup check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// FFmpeg started through `oxrtc-exec` with these limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confinement {
    /// The `oxrtc-exec` binary.
    pub wrapper: PathBuf,
    /// The limits applied to FFmpeg.
    pub limits: Limits,
}

/// Whether, and how, FFmpeg is confined on this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// Not Linux: FFmpeg starts directly (on Windows a job object ties it to
    /// oxrtc).
    NotHere,
    /// `oxrtc-exec` is beside oxrtc.
    Found(Confinement),
    /// Linux, but `oxrtc-exec` is not at this path: FFmpeg would run
    /// unconfined, and outlive a killed oxrtc.
    Missing(PathBuf),
}

impl Confinement {
    /// `oxrtc-exec` with FFmpeg's limits.
    pub fn new(wrapper: PathBuf) -> Self {
        Self { wrapper, limits: FFMPEG_LIMITS }
    }

    /// Looks for `oxrtc-exec` beside `exe` (oxrtc's own path), on Linux only.
    pub fn discover(exe: &Path) -> Discovery {
        Self::discover_on(cfg!(target_os = "linux"), exe)
    }

    fn discover_on(linux: bool, exe: &Path) -> Discovery {
        if !linux {
            return Discovery::NotHere;
        }
        let wrapper = exe.with_file_name(oxrtc_exec::FILE_NAME);
        if wrapper.is_file() { Discovery::Found(Self::new(wrapper)) } else { Discovery::Missing(wrapper) }
    }

    /// The wrapper's command line for `bin` (its arguments follow), with the
    /// environment FFmpeg will have.
    pub(crate) fn plan(&self, bin: &Path, env: &[(String, String)]) -> Plan {
        let var = |name: &str| env.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
        let program = resolve(bin, var("PATH"));
        let mut read: Vec<PathBuf> = READ.iter().map(PathBuf::from).collect();
        // Its own directory, through any symbolic link (a portable build
        // keeps its libraries beside it), and the libraries it is told of.
        let real = std::fs::canonicalize(&program).ok();
        for path in [Some(program.as_path()), real.as_deref()].into_iter().flatten() {
            if let Some(dir) = path.parent().filter(|dir| dir.is_absolute()) {
                read.push(dir.to_owned());
            }
        }
        read.extend(var("LD_LIBRARY_PATH").into_iter().flat_map(std::env::split_paths).filter(|p| p.is_absolute()));
        read.dedup();
        Plan {
            check: false,
            parent: std::process::id(),
            limits: self.limits,
            read,
            write: WRITE.iter().map(PathBuf::from).collect(),
            connect: CONNECT.iter().map(PathBuf::from).collect(),
            program: Some(program),
            args: Vec::new(),
        }
    }

    /// The program and arguments that start `bin` with `args`, confined.
    pub(crate) fn wrap(&self, bin: &Path, args: &[String], env: &[(String, String)]) -> (PathBuf, Vec<OsString>) {
        let mut argv = self.plan(bin, env).to_args();
        argv.extend(args.iter().map(OsString::from));
        (self.wrapper.clone(), argv)
    }

    /// Runs `oxrtc-exec --check` with FFmpeg's plan: what the confinement
    /// becomes on this kernel, or why it cannot be applied (then every
    /// FFmpeg would be refused the same way).
    pub async fn check(&self, bin: &Path) -> Result<Landlock, String> {
        let env = crate::process::kept_env(std::env::vars());
        let mut plan = self.plan(bin, &env);
        plan.check = true;
        plan.program = None;
        let output = tokio::process::Command::new(&self.wrapper)
            .args(plan.to_args())
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(CHECK_TIMEOUT, output)
            .await
            .map_err(|_| format!("no answer within {CHECK_TIMEOUT:?}"))?
            .map_err(|e| format!("cannot run {}: {e}", self.wrapper.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        match Landlock::from_summary(&stdout) {
            Some(state) if output.status.success() => Ok(state),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("{} ({})", stderr.trim(), output.status))
            }
        }
    }
}

/// `bin` as the wrapper will run it: a bare name is looked up in `path`, as
/// FFmpeg would be found if started directly; anything else is kept.
fn resolve(bin: &Path, path: Option<&str>) -> PathBuf {
    if bin.components().count() != 1 || !bin.is_relative() {
        return bin.to_owned();
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_absolute() && candidate.is_file())
        .unwrap_or_else(|| bin.to_owned())
}

#[cfg(test)]
#[path = "confine_tests.rs"]
mod tests;
