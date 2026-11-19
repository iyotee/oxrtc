//! Which console an FFmpeg is started with on Windows (W4, D36).
//!
//! FFmpeg is a console program. Started with `CREATE_NO_WINDOW` it is given a
//! console of its own with no window, and each such console is a `conhost.exe`
//! of about 7 MiB beside it: eight transcodes were eight hosts, 56 MiB of the
//! process tree that go2rtc's tree does not have (its FFmpegs share go2rtc's
//! console). Started with `DETACHED_PROCESS` it has no console at all, which
//! it does not need: its input and output are pipes and sockets, its error
//! output a pipe.
//!
//! A launcher is the exception. Chocolatey's `ffmpeg.exe` shim (what a bare
//! `ffmpeg` resolves to on such a machine) and Scoop's start the real FFmpeg
//! as their own child; detached, the launcher has no console to hand down and
//! the system opens a new one, window included, for every run. So a binary is
//! detached only when its path says it is FFmpeg itself: absolute, and not in
//! a launcher's directory. A bare name, which the system resolves through
//! `PATH` to whatever comes first, keeps its hidden console.

use std::path::Path;

/// `CREATE_NO_WINDOW`: a console of its own, with no window.
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `DETACHED_PROCESS`: no console.
pub(crate) const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The process creation flags for `bin`.
pub(crate) fn creation_flags(bin: &Path) -> u32 {
    if bin.is_absolute() && !is_launcher(bin) { DETACHED_PROCESS } else { CREATE_NO_WINDOW }
}

/// Whether `bin` is a launcher that starts the real program as its child:
/// in Chocolatey's `chocolatey\bin`, or with Scoop's `<name>.shim` beside it.
fn is_launcher(bin: &Path) -> bool {
    let named = |path: Option<&Path>, name: &str| {
        path.and_then(Path::file_name).is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
    };
    let dir = bin.parent();
    if named(dir, "bin") && named(dir.and_then(Path::parent), "chocolatey") {
        return true;
    }
    named(dir, "shims") || bin.with_extension("shim").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_binary_by_its_path_is_detached() {
        let real = Path::new("C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe");
        assert_eq!(creation_flags(real), DETACHED_PROCESS);
    }

    #[test]
    fn a_launcher_or_a_bare_name_keeps_a_hidden_console() {
        for launcher in [
            "C:/ProgramData/chocolatey/bin/ffmpeg.exe",
            "C:/ProgramData/Chocolatey/BIN/ffmpeg.exe",
            "C:/Users/someone/scoop/shims/ffmpeg.exe",
        ] {
            assert_eq!(creation_flags(Path::new(launcher)), CREATE_NO_WINDOW, "{launcher}");
        }
        assert_eq!(creation_flags(Path::new("ffmpeg")), CREATE_NO_WINDOW);
        assert_eq!(creation_flags(Path::new("tools/ffmpeg.exe")), CREATE_NO_WINDOW);
    }

    /// The processes whose parent is `pid`, by name, as the system lists them.
    fn children_of(pid: u32) -> Vec<String> {
        let filter = format!("Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | ForEach-Object Name");
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &filter])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// On the real path: FFmpeg started as a transcode is, with nothing beside
    /// it. With `CREATE_NO_WINDOW` it had a `conhost.exe` child.
    #[tokio::test]
    async fn ffmpeg_runs_with_no_console_host_beside_it() {
        use std::process::Stdio;
        let default = "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe";
        let bin = std::path::PathBuf::from(std::env::var("OXRTC_FFMPEG").unwrap_or_else(|_| default.to_owned()));
        if !bin.is_file() {
            crate::skip(&format!("{} not found (set OXRTC_FFMPEG)", bin.display()));
            return;
        }
        let bin = std::path::absolute(&bin).unwrap();
        let args: Vec<String> = [
            "-hide_banner",
            "-v",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=16x16:r=5",
            "-t",
            "20",
            "-f",
            "null",
            "-",
        ]
        .map(String::from)
        .to_vec();
        let config = crate::FfmpegConfig { bin: bin.clone(), ..crate::FfmpegConfig::default() };
        let launch = crate::process::Launch::new(&config);
        let mut process =
            crate::process::spawn(crate::process::command(&launch, &args, Stdio::null(), Stdio::null()), &bin)
                .await
                .unwrap();
        let pid = process.child.id().unwrap();
        // The host, when there is one, is started with the process.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let children = children_of(pid);
        process.kill();
        assert!(!children.iter().any(|name| name == "conhost.exe"), "FFmpeg has a console host: {children:?}");
    }

    #[test]
    fn a_shim_file_beside_the_binary_marks_a_launcher() {
        let dir = std::env::temp_dir().join(format!("oxrtc-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("ffmpeg.exe");
        assert_eq!(creation_flags(&bin), DETACHED_PROCESS);
        std::fs::write(dir.join("ffmpeg.shim"), "path = elsewhere").unwrap();
        assert_eq!(creation_flags(&bin), CREATE_NO_WINDOW);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
