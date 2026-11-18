//! The confinement, applied for real (Linux only: the Raspberry Pi and CI run
//! these; Windows builds the binary and checks its refusals in `cli.rs`).
#![cfg(target_os = "linux")]
// Helpers of a test: a failure is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oxrtc_exec::Landlock;

const EXEC: &str = env!("CARGO_BIN_EXE_oxrtc-exec");

/// What `/bin/sh` and `/bin/sleep` need: their binaries and libraries, the
/// loader's cache, the process's own `/proc` entry; `/dev/null` written.
fn plan_args(parent: u32) -> Vec<String> {
    let mut args = vec!["--parent".to_owned(), parent.to_string()];
    for path in ["/usr", "/bin", "/lib", "/lib64", "/lib32", "/etc/ld.so.cache", "/etc/ld.so.preload", "/proc/self"] {
        args.extend(["--read".to_owned(), path.to_owned()]);
    }
    args.extend(["--write".to_owned(), "/dev/null".to_owned()]);
    args
}

/// What the ruleset becomes on this kernel.
fn landlock() -> Landlock {
    let output = Command::new(EXEC).arg("--check").args(plan_args(std::process::id())).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let state = Landlock::from_summary(&String::from_utf8_lossy(&output.stdout)).expect("a summary line");
    if state == Landlock::None {
        assert!(String::from_utf8_lossy(&output.stderr).contains("no Landlock"), "the warning");
    }
    state
}

/// The niceness in a `/proc/<pid>/stat` line (field 19).
fn niceness(stat: &str) -> i32 {
    let after_name = &stat[stat.rfind(')').unwrap() + 1..];
    after_name.split_whitespace().nth(16).unwrap().parse().unwrap()
}

/// The soft and hard values of a `/proc/<pid>/limits` row.
fn limit<'a>(limits: &'a str, row: &str) -> Vec<&'a str> {
    let line = limits.lines().find(|l| l.starts_with(row)).unwrap_or_else(|| panic!("no {row} in {limits}"));
    line[row.len()..].split_whitespace().take(2).collect()
}

#[test]
fn the_program_runs_with_every_restriction() {
    let secret = std::env::temp_dir().join(format!("oxrtc-exec-secret-{}", std::process::id()));
    std::fs::write(&secret, "secret\n").unwrap();
    // Built-ins only: /proc/self is the wrapper's own entry, and the shell is
    // the wrapper after exec().
    let script = format!(
        "while IFS= read -r l; do printf '%s\\n' \"$l\"; done < /proc/self/limits\n\
         while IFS= read -r l; do printf '%s\\n' \"$l\"; done < /proc/self/status\n\
         read -r stat < /proc/self/stat; printf 'stat %s\\n' \"$stat\"\n\
         if read -r x < '{}' 2>/dev/null; then echo secret=read; else echo secret=denied; fi\n",
        secret.display()
    );
    let output = Command::new(EXEC)
        .args(plan_args(std::process::id()))
        .args(["--max-address-space", "1073741824", "--max-file-size", "0", "--max-open-files", "64", "--nice", "3"])
        .args(["--", "/bin/sh", "-c", &script])
        .output()
        .unwrap();
    std::fs::remove_file(&secret).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));

    assert!(stdout.lines().any(|l| l.split_whitespace().eq(["NoNewPrivs:", "1"])), "{stdout}");
    assert_eq!(limit(&stdout, "Max open files"), ["64", "64"]);
    assert_eq!(limit(&stdout, "Max file size"), ["0", "0"]);
    assert_eq!(limit(&stdout, "Max core file size"), ["0", "0"]);
    assert_eq!(limit(&stdout, "Max address space"), ["1073741824", "1073741824"]);
    let own = niceness(&std::fs::read_to_string("/proc/self/stat").unwrap());
    let stat = stdout.lines().find_map(|l| l.strip_prefix("stat ")).unwrap();
    assert_eq!(niceness(stat), (own + 3).min(19));

    match landlock() {
        // A kernel without Landlock restricts no path, and the check is left
        // out with a line nobody reads in a passing test. Where
        // `OXRTC_NO_SKIP` is set, as CI sets it, that is a failure: its
        // kernels have Landlock, the arm64 one too, and a green job is what
        // says FFmpeg's files were confined there.
        Landlock::None => {
            let must = std::env::var_os("OXRTC_NO_SKIP").is_some();
            assert!(!must, "no Landlock on this kernel: the path restriction is not checked (OXRTC_NO_SKIP is set)");
            eprintln!("NOTE: no Landlock on this kernel, the path restriction is not checked");
        }
        _ => assert!(stdout.contains("secret=denied"), "a file outside the plan was read: {stdout}"),
    }
}

/// `/proc/<pid>` is gone or a zombie.
fn dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat[stat.rfind(')').unwrap() + 1..].trim_start().starts_with('Z'),
    }
}

/// R13: the program dies with the process that started it, even on SIGKILL.
#[test]
fn the_program_dies_with_its_parent_killed() {
    let reads = plan_args(1).split_off(2).join(" ");
    // The shell stands for oxrtc: it starts the wrapper, prints its pid and
    // waits. `$$` is the shell, the wrapper's parent.
    let script = format!("\"$0\" --parent $$ {reads} -- /bin/sleep 30 & echo $!; wait");
    let mut parent = Command::new("/bin/sh")
        .args(["-c", &script, EXEC])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(parent.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let pid: u32 = line.trim().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim() != "sleep" {
        assert!(Instant::now() < deadline, "the wrapper never became sleep");
        std::thread::sleep(Duration::from_millis(10));
    }

    parent.kill().unwrap(); // SIGKILL: no handler, no clean-up.
    let killed = Instant::now();
    parent.wait().unwrap();
    while !dead(pid) {
        assert!(killed.elapsed() < Duration::from_secs(1), "the program outlived its killed parent by 1 s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_parent_already_gone_runs_nothing() {
    let output = Command::new(EXEC).args(plan_args(1)).args(["--", "/bin/echo", "ran"]).output().unwrap();
    assert_eq!(output.status.code(), Some(i32::from(oxrtc_exec::REFUSED)));
    assert!(String::from_utf8_lossy(&output.stderr).contains("parent 1 is gone"));
    assert!(output.stdout.is_empty(), "nothing ran");
}

#[test]
fn a_missing_program_says_so() {
    let output =
        Command::new(EXEC).args(plan_args(std::process::id())).args(["--", "/nonexistent/ffmpeg"]).output().unwrap();
    assert_eq!(output.status.code(), Some(i32::from(oxrtc_exec::NOT_FOUND)));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot run /nonexistent/ffmpeg"));
}
