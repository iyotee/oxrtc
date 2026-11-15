//! The refusals, on every platform. (Also what makes `cargo test` build the
//! binary that oxrtc's own tests find beside theirs.)

use std::process::Command;

const EXEC: &str = env!("CARGO_BIN_EXE_oxrtc-exec");

#[test]
fn a_bad_command_line_runs_nothing() {
    for args in [&[][..], &["--", "true"], &["--parent", "1", "--sh", "--", "true"]] {
        let output = Command::new(EXEC).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(i32::from(oxrtc_exec::REFUSED)), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with("oxrtc-exec: "), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty());
    }
}

/// Elsewhere than Linux it runs nothing at all: oxrtc starts FFmpeg itself.
#[cfg(not(target_os = "linux"))]
#[test]
fn elsewhere_than_linux_it_refuses() {
    let parent = std::process::id().to_string();
    let output = Command::new(EXEC).args(["--parent", &parent, "--", "cmd", "/c", "echo ran"]).output().unwrap();
    assert_eq!(output.status.code(), Some(i32::from(oxrtc_exec::REFUSED)));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Linux only"));
    assert!(output.stdout.is_empty(), "nothing ran");
}
