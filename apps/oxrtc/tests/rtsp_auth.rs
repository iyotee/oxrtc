//! The binary with `rtsp.username`/`rtsp.password` set, on openIVR's own stream
//! layout (R28, D24): readers must authenticate (loopback ones too), the
//! server's own `rtsp://127.0.0.1:<port>/...` sources between streams do not
//! (they are in-process), and the FFmpeg children read with a credential of
//! their own, never the operator's. Skipped (and passed) when FFmpeg or ffprobe
//! is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::launch::wait_healthy;
use common::*;

const USER: &str = "admin";
const PASS: &str = "s3cret-api";
const RTSP_USER: &str = "nvr";
const RTSP_PASS: &str = "rtsp-s3cret";
const RELAY_STALL: u32 = 20;

fn write_config(
    dir: &std::path::Path,
    tools: &Tools,
    camera: &FakeCamera,
    ports: (u16, u16, u16),
    rtsp_extra: &str,
) -> std::path::PathBuf {
    let (api, rtsp, webrtc) = ports;
    let src = "camera_1_src";
    let video = format!("rtsp://127.0.0.1:{rtsp}/{src}?video#timeout={RELAY_STALL}");
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{USER}\"\n  password: \"{PASS}\"\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n  username: \"{RTSP_USER}\"\n  password: \"{RTSP_PASS}\"\n{rtsp_extra}\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         ffmpeg:\n  bin: \"{ffmpeg}\"\n\
         log:\n  level: info\n\
         streams:\n\
         \x20 {src}: [\"{camera}#timeout=10\"]\n\
         \x20 camera_1_rec: [\"{video}\", \"ffmpeg:rtsp://127.0.0.1:{rtsp}/{src}#audio=opus#timeout={RELAY_STALL}\"]\n\
         \x20 camera_1: [\"{video}\", \"ffmpeg:{src}#video=h264#width=1920#height=1080#timeout={RELAY_STALL}\", \
         \"rtsp://127.0.0.1:{rtsp}/camera_1_rec?audio=opus#timeout={RELAY_STALL}\"]\n",
        ffmpeg = tools.ffmpeg.display(),
        camera = camera.url("with_sound"),
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

/// The command line of process `pid`: sysinfo reads none on Windows, so
/// PowerShell does (`Win32_Process`); Linux has it in `/proc`.
fn command_line(pid: u32) -> String {
    if cfg!(windows) {
        let script = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine");
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
    } else {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
            .unwrap_or_default()
    }
}

/// The command lines of every process `parent` started that was given an input.
fn children_commands(parent: u32) -> Vec<String> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    let parent = sysinfo::Pid::from_u32(parent);
    let pids: Vec<u32> =
        system.processes().iter().filter(|(_, p)| p.parent() == Some(parent)).map(|(pid, _)| pid.as_u32()).collect();
    pids.into_iter().map(command_line).filter(|cmd| cmd.contains(" -i ")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_authenticate_and_the_transcoders_read_with_a_credential_of_their_own() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let dir = temp_dir("rtsp-auth");
    let ports = (free_tcp_port(), free_tcp_port(), free_webrtc_port());
    let (api_port, rtsp_port) = (ports.0, ports.1);
    let config = write_config(&dir, &tools, &camera, ports, "");
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;

    // The NVR's hold, then what the recorder reads: the credential is needed ...
    let held = Request::new("PUT", "/api/preload?src=camera_1_rec&audio=opus").auth(USER, PASS).send(api).await;
    assert_eq!(held.status, 200, "{}", held.text());
    let bare = format!("rtsp://127.0.0.1:{rtsp_port}/camera_1_rec");
    assert_eq!(codecs(&tools, &bare).await, "", "a reader with no credential is refused, loopback or not");
    let wrong = format!("rtsp://{RTSP_USER}:nope@127.0.0.1:{rtsp_port}/camera_1_rec");
    assert_eq!(codecs(&tools, &wrong).await, "");
    // ... and the operator's opens it, to the sound the transcoder made.
    let given = format!("rtsp://{RTSP_USER}:{RTSP_PASS}@127.0.0.1:{rtsp_port}/camera_1_rec");
    assert_eq!(codecs(&tools, &given).await, "h264,opus", "camera_1_rec with the operator's credential");

    // The transcoders read the relay with credentials of their own.
    let commands = children_commands(oxrtc.child.id().unwrap());
    eprintln!("{} transcoder command lines", commands.len());
    assert!(!commands.is_empty(), "the sound transcode is running");
    let mut own = 0;
    for command in &commands {
        assert!(!command.contains(RTSP_PASS) && !command.contains(RTSP_USER), "the operator's credential: {command}");
        if command.contains("://run-") {
            own += 1;
        }
    }
    assert!(own >= 1, "a transcoder reads the relay as a run: {commands:?}");

    // The viewer's stream chains the in-process aliases and a transcoder, with no credential written anywhere.
    let sound = whep::play(api, (USER, PASS), "camera_1", true, 20, Duration::from_secs(25)).await;
    assert_eq!(sound.status, 201, "{sound:?}");
    assert!(sound.video_frames >= 20, "{sound:?}");
    assert!(sound.codecs.contains(&str0m::format::Codec::Opus), "the viewer hears the camera: {sound:?}");

    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let log = oxrtc.drain().await;
    camera.stop().await;
    remove_dir(&dir).await;
    for line in &log {
        assert!(
            !line.contains(RTSP_PASS) && !line.contains(PASS) && !line.contains(CAMERA_PASS),
            "a password in the log: {line}"
        );
        assert!(!line.contains("://run-"), "a run's credential in the log: {line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_auth_false_gives_go2rtcs_loopback_rule_back() {
    let Some(tools) = Tools::find() else { return };
    let camera = FakeCamera::start_with_sound(&tools).await;
    let dir = temp_dir("rtsp-auth-local");
    let ports = (free_tcp_port(), free_tcp_port(), free_webrtc_port());
    let (api_port, rtsp_port) = (ports.0, ports.1);
    let config = write_config(&dir, &tools, &camera, ports, "  local_auth: false\n");
    let api = SocketAddr::from(([127, 0, 0, 1], api_port));
    let oxrtc = Oxrtc::start(&config);
    wait_healthy(api).await;
    let held = Request::new("PUT", "/api/preload?src=camera_1_rec&audio=opus").auth(USER, PASS).send(api).await;
    assert_eq!(held.status, 200, "{}", held.text());
    let bare = format!("rtsp://127.0.0.1:{rtsp_port}/camera_1_rec");
    assert_eq!(codecs(&tools, &bare).await, "h264,opus", "loopback is let in, as on go2rtc");
    // The transcoders keep their own credential (the relay may be reached from a
    // non-loopback address), and still never the operator's.
    let commands = children_commands(oxrtc.child.id().unwrap());
    assert!(commands.iter().all(|c| !c.contains(RTSP_PASS)), "{commands:?}");
    let mut oxrtc = oxrtc;
    oxrtc.child.kill().await.unwrap();
    let _ = oxrtc.drain().await;
    camera.stop().await;
    remove_dir(&dir).await;
}
