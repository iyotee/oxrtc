//! Running FFmpeg: an argument vector (no shell), a cleared environment, no
//! console window, killed with its owner, its error output drained into the
//! log - redacted, because an FFmpeg error line quotes its input URL.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::FfmpegConfig;
use crate::confine::Confinement;

/// Environment variables FFmpeg is given; everything else is cleared, so the
/// child never sees our secrets. `PATH` resolves a bare `ffmpeg`; Windows needs
/// `SystemRoot` for its socket and crypto libraries; a portable build may need
/// its library path.
const KEPT_ENV: [&str; 5] = ["PATH", "SystemRoot", "windir", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"];

/// `CREATE_NO_WINDOW`: no console flashes up for each process.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Longest error line kept; the rest is cut.
const MAX_LINE: usize = 512;

/// Error lines remembered, to say why a process ended.
const TAIL_LINES: usize = 4;

/// The variables of `vars` that FFmpeg may see.
pub(crate) fn kept_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.filter(|(key, _)| KEPT_ENV.iter().any(|kept| kept.eq_ignore_ascii_case(key))).collect()
}

/// How FFmpeg is started: its binary, directly or through `oxrtc-exec`.
#[derive(Debug, Clone)]
pub(crate) struct Launch {
    pub(crate) bin: PathBuf,
    confine: Option<Confinement>,
}

impl Launch {
    pub(crate) fn new(config: &FfmpegConfig) -> Self {
        Self { bin: config.bin.clone(), confine: config.confine.clone() }
    }
}

/// A command for FFmpeg with `args`: stdin as given, stdout as given, stderr
/// piped; through `oxrtc-exec` when confined.
pub(crate) fn command(launch: &Launch, args: &[String], stdin: Stdio, stdout: Stdio) -> Command {
    let env = kept_env(std::env::vars());
    let mut command = match &launch.confine {
        Some(confine) => {
            let (wrapper, argv) = confine.wrap(&launch.bin, args, &env);
            let mut command = Command::new(wrapper);
            command.args(argv);
            command
        }
        None => {
            let mut command = Command::new(&launch.bin);
            command.args(args);
            command
        }
    };
    command.env_clear().envs(env).stdin(stdin).stdout(stdout).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Starts `command` (from the one starting thread, [`crate::spawner`]),
/// saying why it could not be.
pub(crate) async fn spawn(command: Command, bin: &Path) -> Result<Process, String> {
    let child = crate::spawner::spawn(command)
        .await
        .map_err(|e| format!("cannot start {}: {e}", oxrtc_core::spec::redact(&bin.display().to_string())))?;
    let tree = Tree::hold(&child);
    Ok(Process { child, tree })
}

/// A started FFmpeg. Dropping it kills the process (`kill_on_drop`) and, on
/// Windows, every process it started: `bin` may be a launcher (Chocolatey's
/// `ffmpeg.exe` shim, which a bare `ffmpeg` resolves to there) whose death
/// alone would leave the real FFmpeg running, outside the process cap. The
/// job object also dies with oxrtc itself, crash included (the system closes
/// its handle), so no FFmpeg outlives its owner. On Linux the same holds
/// through `oxrtc-exec` (its death signal), when [`Launch`] is confined.
#[derive(Debug)]
pub(crate) struct Process {
    pub(crate) child: Child,
    // Declared after `child`, so dropped after it.
    tree: Tree,
}

impl Process {
    /// Kills the process and everything it started, without waiting.
    pub(crate) fn kill(&mut self) {
        let _ = self.child.start_kill();
        self.tree.kill();
    }
}

/// The job object holding a process and its descendants (Windows).
#[cfg(windows)]
#[derive(Debug)]
struct Tree {
    // Closing it kills the processes in it.
    job: Option<win32job::Job>,
}

#[cfg(windows)]
impl Tree {
    fn hold(child: &Child) -> Self {
        let job = (|| {
            let mut info = win32job::ExtendedLimitInfo::new();
            info.limit_kill_on_job_close();
            let job = win32job::Job::create_with_limit_info(&info).ok()?;
            // A launcher could start its child before this line; it does not
            // in practice (process start-up takes milliseconds, this microseconds).
            job.assign_process(child.raw_handle()? as isize).ok()?;
            Some(job)
        })();
        if job.is_none() {
            tracing::warn!("ffmpeg: no job object, only the direct child is killed with its owner");
        }
        Self { job }
    }

    fn kill(&mut self) {
        self.job = None;
    }
}

/// Elsewhere the direct child only (`kill_on_drop`); oxrtc's own death is
/// `oxrtc-exec`'s death signal on Linux, which `exec()`s FFmpeg (one process,
/// no launcher in between).
#[cfg(not(windows))]
#[derive(Debug)]
struct Tree;

#[cfg(not(windows))]
impl Tree {
    fn hold(_child: &Child) -> Self {
        Self
    }

    fn kill(&mut self) {}
}

/// The last error lines of a process, redacted.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tail(Arc<Mutex<VecDeque<String>>>);

impl Tail {
    fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() >= TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The last line, if any.
    #[cfg(test)]
    pub(crate) fn last(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).back().cloned()
    }

    /// The remembered lines, oldest first, on one line: at most
    /// [`TAIL_LINES`] of [`MAX_LINE`] bytes.
    pub(crate) fn joined(&self) -> Option<String> {
        let lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (!lines.is_empty()).then(|| lines.iter().map(String::as_str).collect::<Vec<_>>().join(" | "))
    }

    /// Every remembered line, oldest first.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect()
    }
}

/// Reads `stderr` to its end, logging each line at debug level, redacted and
/// cut to [`MAX_LINE`] bytes, and remembering the last ones in `tail`.
/// Memory stays bounded whatever FFmpeg writes.
pub(crate) async fn drain(mut stderr: impl AsyncRead + Unpin, tail: Tail, label: &str) {
    let mut chunk = [0u8; 4096];
    let mut line: Vec<u8> = Vec::with_capacity(MAX_LINE);
    let mut overflow = false;
    loop {
        let n = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &byte in chunk.get(..n).unwrap_or_default() {
            if byte == b'\n' || byte == b'\r' {
                emit(&mut line, &mut overflow, &tail, label);
            } else if line.len() < MAX_LINE {
                line.push(byte);
            } else {
                overflow = true;
            }
        }
    }
    emit(&mut line, &mut overflow, &tail, label);
}

/// A line with FFmpeg's context addresses left out: `[rtsp @ 000001f97d146600]`
/// is `[rtsp]`. The address is a new one at every run, so with it no two
/// failures of a source read the same, and the supervisor's warning, said
/// again only when the error changes, was said at every attempt.
pub(crate) fn without_addresses(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(" @ ") {
        let after = &rest[at + 3..];
        let hex = after.strip_prefix("0x").unwrap_or(after);
        let digits = hex.bytes().take_while(u8::is_ascii_hexdigit).count();
        if digits >= 6 && hex[digits..].starts_with(']') {
            out.push_str(&rest[..at]);
            rest = &hex[digits..];
        } else {
            out.push_str(&rest[..at + 3]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn emit(line: &mut Vec<u8>, overflow: &mut bool, tail: &Tail, label: &str) {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if !text.is_empty() {
        let mut shown = without_addresses(&oxrtc_core::spec::redact(text));
        if *overflow {
            shown.push_str(" [...]");
        }
        tracing::debug!(source = label, "ffmpeg: {shown}");
        tail.push(shown);
    }
    line.clear();
    *overflow = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_output_is_redacted_cut_and_bounded() {
        let mut input = Vec::new();
        input.extend_from_slice(b"[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized\r\n");
        input.extend_from_slice(b"rtsp://admin:hunter2@192.168.1.10:554/live: Server returned 401\n");
        input.extend_from_slice(&[b'x'; 5000]);
        input.extend_from_slice(b"\nhttp://cam/snap?password=topsecret: Connection refused");
        let tail = Tail::default();
        drain(&input[..], tail.clone(), "test").await;
        let lines = tail.lines();
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|l| !l.contains("hunter2") && !l.contains("topsecret")), "{lines:?}");
        assert_eq!(lines[1], "rtsp://***@192.168.1.10:554/live: Server returned 401");
        assert_eq!(lines[2].len(), MAX_LINE + " [...]".len());
        // A secret query value is hidden to its end: over-redaction is accepted.
        assert_eq!(tail.last().unwrap(), "http://cam/snap?password=***");
    }

    /// Two runs of the same failing input, as FFmpeg 8 writes them on the
    /// owner's PC (2026-11-17): the same words, a new context address each.
    #[tokio::test]
    async fn the_context_addresses_are_left_out_so_two_runs_read_the_same() {
        let run = |a: &str, b: &str| {
            format!(
                "[rtsp @ {a}] method PLAY failed: 455Method Not Valid In This State\n\
                 [in#0 @ {b}] Error opening input: Server returned 4XX Client Error, but not one of 40{{0,1,3,4}}\n"
            )
        };
        let mut seen = Vec::new();
        for (a, b) in [("000001f97d146600", "000001f97d144bc0"), ("0000020aa1a36600", "0x20aa1a34bc0")] {
            let tail = Tail::default();
            drain(run(a, b).as_bytes(), tail.clone(), "test").await;
            seen.push(tail.joined().unwrap());
        }
        assert_eq!(seen[0], seen[1]);
        assert_eq!(
            seen[0],
            "[rtsp] method PLAY failed: 455Method Not Valid In This State | \
             [in#0] Error opening input: Server returned 4XX Client Error, but not one of 40{0,1,3,4}"
        );
        // What is not an address is kept: a short one, an e-mail-like word, a bare " @ ".
        assert_eq!(without_addresses("[rtsp @ 0x1] a @ b user @ deadbeefzz]"), "[rtsp @ 0x1] a @ b user @ deadbeefzz]");
    }

    #[tokio::test]
    async fn the_tail_keeps_only_the_last_lines() {
        let input: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let tail = Tail::default();
        drain(input.as_bytes(), tail.clone(), "test").await;
        assert_eq!(tail.lines(), ["line 96", "line 97", "line 98", "line 99"]);
        assert_eq!(tail.joined().unwrap(), "line 96 | line 97 | line 98 | line 99");
        assert_eq!(Tail::default().joined(), None);
    }

    /// Confined, the wrapper starts with FFmpeg's plan, then FFmpeg and its
    /// arguments as they were; the environment is FFmpeg's either way.
    #[test]
    fn a_confined_command_starts_the_wrapper_then_ffmpeg() {
        let bin = PathBuf::from("/usr/bin/ffmpeg");
        let args = vec!["-i".to_owned(), "rtsp://user:secret@cam/live".to_owned()];
        let confine = Some(Confinement::new("/opt/oxrtc/oxrtc-exec".into()));
        let confined = Launch::new(&FfmpegConfig { bin: bin.clone(), confine, ..FfmpegConfig::default() });
        let command = command(&confined, &args, Stdio::null(), Stdio::null());
        let std = command.as_std();
        assert_eq!(Path::new(std.get_program()), Path::new("/opt/oxrtc/oxrtc-exec"));
        let plan = oxrtc_exec::Plan::parse(std.get_args().map(std::ffi::OsStr::to_owned)).unwrap();
        assert_eq!(plan.program.as_deref(), Some(bin.as_path()));
        assert_eq!(plan.args, args.iter().map(std::ffi::OsString::from).collect::<Vec<_>>());
        assert_eq!(plan.parent, std::process::id());
        let env_kept = |command: &Command| {
            command.as_std().get_envs().all(|(key, _)| {
                KEPT_ENV.iter().any(|kept| key.to_str().is_some_and(|key| kept.eq_ignore_ascii_case(key)))
            })
        };
        assert!(env_kept(&command));

        let direct = Launch::new(&FfmpegConfig { bin: bin.clone(), ..FfmpegConfig::default() });
        let command = super::command(&direct, &args, Stdio::null(), Stdio::null());
        assert_eq!(Path::new(command.as_std().get_program()), bin);
        assert!(command.as_std().get_args().eq(args.iter().map(std::ffi::OsStr::new)));
        assert!(env_kept(&command));
    }

    #[test]
    fn only_the_needed_environment_is_kept() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("Path", "C:/Windows"),
            ("SYSTEMROOT", "C:/Windows"),
            ("OXRTC_API_PASSWORD", "secret"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("HOME", "/root"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let kept: Vec<String> = kept_env(vars.into_iter()).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kept, ["PATH", "Path", "SYSTEMROOT"]);
    }

    /// FFmpeg from `var`, or its default path; `None` (skip) if absent.
    fn binary(var: &str, default: &str) -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_owned()));
        if path.is_file() {
            Some(path)
        } else {
            crate::skip(&format!("{} not found (set {var})", path.display()));
            None
        }
    }

    /// Starts `bin` sending a test pattern to a listener of ours, drops the
    /// handle once it is connected, and says whether the connection closed
    /// soon after - that is, whether the process that holds it died.
    async fn dies_with_its_handle(bin: &Path) -> bool {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let output = format!("tcp://{}", listener.local_addr().unwrap());
        let args: Vec<String> = [
            "-hide_banner",
            "-v",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=5",
            "-t",
            "60",
            "-f",
            "mpegts",
            &output,
        ]
        .map(String::from)
        .to_vec();
        let launch = Launch { bin: bin.to_owned(), confine: None };
        let process = spawn(command(&launch, &args, Stdio::null(), Stdio::null()), bin).await.unwrap();
        let (mut socket, _) =
            tokio::time::timeout(Duration::from_secs(20), listener.accept()).await.expect("never connected").unwrap();
        drop(process);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout_at(deadline, socket.read(&mut buf)).await {
                Err(_) => return false,
                Ok(Ok(0) | Err(_)) => return true,
                Ok(Ok(_)) => {}
            }
        }
    }

    #[tokio::test]
    async fn dropping_the_handle_kills_ffmpeg() {
        let default = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
        let Some(bin) = binary("OXRTC_FFMPEG", default) else { return };
        assert!(dies_with_its_handle(&bin).await, "ffmpeg outlived its handle");
    }

    /// A launcher that starts the real FFmpeg as its own child (Chocolatey's
    /// `ffmpeg.exe` shim, which a bare `ffmpeg` resolves to on such a
    /// machine): killing the launcher alone leaves FFmpeg running; the job
    /// object kills both.
    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_the_handle_kills_what_a_launcher_started() {
        let Some(shim) = binary("OXRTC_FFMPEG_LAUNCHER", "C:/ProgramData/chocolatey/bin/ffmpeg.exe") else { return };
        assert!(dies_with_its_handle(&shim).await, "the FFmpeg started by the launcher outlived its handle");
    }
}
