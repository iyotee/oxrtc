use super::*;

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn env_with(path: &[&Path], libs: &[&Path]) -> Vec<(String, String)> {
    let join = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap().into_string().unwrap();
    vec![("PATH".into(), join(path)), ("LD_LIBRARY_PATH".into(), join(libs))]
}

/// `oxrtc-exec` as `cargo test` builds it beside this test's own directory
/// (`target/<profile>/deps/..`); `None` (skip) when only this crate is built.
pub(crate) fn built_wrapper() -> Option<PathBuf> {
    let exe = std::env::current_exe().unwrap();
    let wrapper = exe.parent()?.parent()?.join(oxrtc_exec::FILE_NAME);
    if wrapper.is_file() {
        Some(wrapper)
    } else {
        eprintln!("SKIP: {} not built (cargo test --workspace builds it)", wrapper.display());
        None
    }
}

#[test]
fn the_wrapper_is_looked_for_beside_oxrtc_on_linux_only() {
    let dir = temp();
    let exe = dir.path().join("oxrtc");
    assert_eq!(Confinement::discover_on(false, &exe), Discovery::NotHere);
    let wrapper = dir.path().join(oxrtc_exec::FILE_NAME);
    assert_eq!(Confinement::discover_on(true, &exe), Discovery::Missing(wrapper.clone()));
    std::fs::write(&wrapper, b"").unwrap();
    let Discovery::Found(found) = Confinement::discover_on(true, &exe) else { panic!("not found") };
    assert_eq!(found, Confinement { wrapper, limits: FFMPEG_LIMITS });
    if !cfg!(target_os = "linux") {
        assert_eq!(Confinement::discover(&exe), Discovery::NotHere, "FFmpeg starts directly here");
    }
}

#[test]
fn ffmpegs_limits() {
    let limits = Confinement::new("oxrtc-exec".into()).limits;
    assert_eq!(limits.address_space, Some(4 * 1024 * 1024 * 1024));
    assert_eq!(limits.file_size, Some(0), "FFmpeg writes no file");
    assert_eq!(limits.open_files, Some(256));
    assert_eq!(limits.nice, 2);
}

#[test]
fn a_bare_name_is_found_in_the_path_and_its_directory_readable() {
    let (bins, libs, other) = (temp(), temp(), temp());
    let ffmpeg = bins.path().join("ffmpeg");
    std::fs::write(&ffmpeg, b"").unwrap();
    let confine = Confinement::new("oxrtc-exec".into());
    let plan = confine.plan(Path::new("ffmpeg"), &env_with(&[other.path(), bins.path()], &[libs.path()]));
    assert_eq!(plan.program.as_deref(), Some(ffmpeg.as_path()));
    assert_eq!(plan.parent, std::process::id());
    assert_eq!(plan.limits, FFMPEG_LIMITS);
    assert!(!plan.check);
    assert!(plan.read.iter().any(|p| p == bins.path()), "its directory: {:?}", plan.read);
    assert!(plan.read.iter().any(|p| p == libs.path()), "LD_LIBRARY_PATH: {:?}", plan.read);
    assert!(!plan.read.iter().any(|p| p == other.path()), "only the directory it is in");
    assert_eq!(plan.write, [PathBuf::from("/dev/null")]);
    assert!(plan.connect.iter().any(|p| p == Path::new("/run/systemd/resolve")));

    // Not in the PATH: kept as it is, for exec() to say it is not found.
    let plan = confine.plan(Path::new("ffmpeg"), &env_with(&[other.path()], &[]));
    assert_eq!(plan.program.as_deref(), Some(Path::new("ffmpeg")));
    // A path is never looked up.
    let given = other.path().join("ffmpeg");
    let plan = confine.plan(&given, &env_with(&[bins.path()], &[]));
    assert_eq!(plan.program.as_deref(), Some(given.as_path()));
    assert!(plan.read.iter().any(|p| p == other.path()));
}

/// oxrtc's configuration (credentials), its environment through `/proc`,
/// and the places a compromised FFmpeg would drop a file are not readable.
#[test]
fn the_secrets_are_outside_what_ffmpeg_reads() {
    let plan = Confinement::new("oxrtc-exec".into()).plan(Path::new("/usr/bin/ffmpeg"), &[]);
    for wide in ["/", "/etc", "/proc", "/home", "/root", "/tmp", "/run", "/var", "/opt", "/dev", "/sys"] {
        assert!(!plan.read.iter().any(|p| p == Path::new(wide)), "{wide} is readable");
    }
    for needed in ["/usr", "/lib", "/etc/ssl", "/etc/resolv.conf", "/proc/self", "/dev/urandom"] {
        assert!(plan.read.iter().any(|p| p == Path::new(needed)), "{needed} is not readable");
    }
    if cfg!(unix) {
        assert_eq!(plan.read.iter().filter(|p| *p == Path::new("/usr/bin")).count(), 1, "its directory, once");
    }
}

#[test]
fn the_wrapped_command_line_carries_ffmpegs_arguments_untouched() {
    let confine = Confinement::new("/opt/oxrtc/oxrtc-exec".into());
    let args: Vec<String> =
        ["-i", "rtsp://admin:pa ss@10.0.0.5/live", "--", "--read", "/etc"].map(String::from).to_vec();
    let (program, argv) = confine.wrap(Path::new("/usr/bin/ffmpeg"), &args, &[]);
    assert_eq!(program, PathBuf::from("/opt/oxrtc/oxrtc-exec"));
    let plan = Plan::parse(argv).unwrap();
    assert_eq!(plan.program.as_deref(), Some(Path::new("/usr/bin/ffmpeg")));
    assert_eq!(plan.args, args.iter().map(OsString::from).collect::<Vec<_>>());
    assert!(!plan.read.iter().any(|p| p == Path::new("/etc")), "an argument never widens the plan");
}

/// The startup check runs the real wrapper: on Linux it says what the
/// ruleset became; elsewhere the wrapper refuses, and the check says why.
#[tokio::test]
async fn the_check_reports_what_the_wrapper_says() {
    let Some(wrapper) = built_wrapper() else { return };
    let checked = Confinement::new(wrapper).check(Path::new("ffmpeg")).await;
    if cfg!(target_os = "linux") {
        assert!(checked.is_ok(), "{checked:?}");
    } else {
        let error = checked.unwrap_err();
        assert!(error.contains("Linux only"), "{error}");
    }
    let missing = Confinement::new(temp().path().join("oxrtc-exec")).check(Path::new("ffmpeg")).await;
    assert!(missing.unwrap_err().starts_with("cannot run"));
}

/// The death signal follows the thread that started the process: FFmpeg
/// started while a short-lived thread polls the run must not die with that
/// thread (here a blocking-pool thread that idles out after 50 ms).
#[cfg(target_os = "linux")]
#[test]
fn ffmpeg_outlives_the_thread_that_asked_for_it() {
    use crate::process::{Launch, command, spawn};
    let Some(wrapper) = built_wrapper() else { return };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_keep_alive(Duration::from_millis(50))
        .enable_all()
        .build()
        .unwrap();
    let config = crate::FfmpegConfig {
        bin: "/bin/sleep".into(),
        confine: Some(Confinement::new(wrapper)),
        ..Default::default()
    };
    let launch = Launch::new(&config);
    let process = runtime.block_on(async move {
        tokio::task::spawn_blocking(move || {
            let started = spawn(command(&launch, &["30".to_owned()], Stdio::null(), Stdio::null()), &launch.bin);
            tokio::runtime::Handle::current().block_on(started)
        })
        .await
        .unwrap()
        .unwrap()
    });
    let pid = process.child.id().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("killed with the thread");
    assert!(status.lines().any(|l| l.split_whitespace().eq(["Name:", "sleep"])), "{status}");
    assert!(status.lines().any(|l| l.split_whitespace().eq(["NoNewPrivs:", "1"])), "{status}");
    assert!(!status.lines().any(|l| l.starts_with("State:") && l.contains("zombie")), "{status}");
    runtime.block_on(async move { drop(process) });
}
