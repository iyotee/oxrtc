//! The configuration files openIVR writes, read as they are: three files
//! produced by its own writer (`Go2RtcManager._write_config`, PyYAML flow
//! style, the camera addresses replaced by documentation ones), and one that
//! go2rtc wrote back after an API call (block style, a `preload:` section).
//! They are in `crates/api/tests/fixtures/`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;

use oxrtc::App;
use oxrtc_api::config::{Config, parse};

fn fixture(name: &str) -> String {
    let path: PathBuf =
        [env!("CARGO_MANIFEST_DIR"), "..", "..", "crates", "api", "tests", "fixtures", name].iter().collect();
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The three cameras of the kind openIVR writes: a relay and a recording
/// stream per camera, a public one, a substream for one of them.
const CAMERAS_WITH_A_RELAY: [&str; 5] = ["camera_1", "camera_12", "camera_2", "camera_4", "camera_5"];

fn names(config: &Config) -> Vec<&str> {
    config.streams.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn what_openivr_writes_is_read_whole_and_without_a_warning() {
    for file in ["openivr_written.yaml", "openivr_written_ice.yaml"] {
        let (config, warnings) = parse(&fixture(file)).unwrap();
        assert!(warnings.is_empty(), "{file}: {warnings:?}");
        // The API: loopback, with the derived credential, and `local_auth`.
        assert_eq!(config.api.listen, "127.0.0.1:1984", "{file}");
        assert_eq!(config.api.username.as_deref(), Some("openivr"), "{file}");
        assert_eq!(config.api.password.as_deref(), Some("k3y-d3rived"), "{file}");
        assert_eq!(config.rtsp.listen, "127.0.0.1:8554", "{file}");
        assert_eq!(config.webrtc.listen, ":8555", "{file}");
        assert_eq!(config.webrtc.candidates, ["192.0.2.41"], "{file}");
        assert_eq!(config.log.level, "info", "{file}");
        // A Windows path with its backslashes, and the two templates.
        assert_eq!(
            config.ffmpeg.bin.as_deref().and_then(|p| p.to_str()),
            Some(r"C:\ProgramData\chocolatey\lib\ffmpeg\tools\ffmpeg\bin\ffmpeg.exe"),
            "{file}"
        );
        assert!(config.ffmpeg.templates["h264"].ends_with("-fps_mode:v passthrough"), "{file}");
        assert_eq!(
            config.ffmpeg.templates["dedup_input"], "-fflags nobuffer -flags low_delay -probesize 32 -i {input}",
            "{file}"
        );
        // Every camera: its connection, what is recorded, what is watched.
        let streams = names(&config);
        for camera in CAMERAS_WITH_A_RELAY {
            for suffix in ["", "_rec", "_src"] {
                assert!(streams.contains(&format!("{camera}{suffix}").as_str()), "{file}: {camera}{suffix}");
            }
        }
        assert!(streams.contains(&"camera_1_sub"), "{file}: the substream of the camera that declares one");
        assert_eq!(streams.len(), 5 * 3 + 1, "{file}: {streams:?}");
        let public = &config.streams.iter().find(|(n, _)| n == "camera_1").unwrap().1;
        assert_eq!(public.len(), 3, "the video, the transcode, the sound, in that order: {public:?}");
        assert!(public[0].ends_with("/camera_1_src?video#timeout=20"), "{public:?}");
        assert!(public[1].starts_with("ffmpeg:camera_1_src#video=h264#width=1920#height=1080"), "{public:?}");
        assert!(public[2].contains("/camera_1_rec?audio=opus"), "{public:?}");
    }
}

#[test]
fn the_ice_servers_openivr_writes_are_read_as_one_entry() {
    let (config, _) = parse(&fixture("openivr_written_ice.yaml")).unwrap();
    let [server] = config.webrtc.ice_servers.as_slice() else { panic!("{:?}", config.webrtc.ice_servers) };
    assert_eq!(
        server.urls,
        ["stun:relay.example:3478", "turn:relay.example:3478?transport=udp", "turn:relay.example:3478?transport=tcp"]
    );
    assert_eq!(server.username.as_deref(), Some("1700000000:site"));
    assert_eq!(server.credential.as_deref(), Some("c2VjcmV0"));
    let (plain, _) = parse(&fixture("openivr_written.yaml")).unwrap();
    assert!(plain.webrtc.ice_servers.is_empty(), "a LAN-only installation writes none");
}

#[test]
fn a_file_go2rtc_wrote_back_reads_the_same_streams() {
    let (written, _) = parse(&fixture("openivr_written.yaml")).unwrap();
    let (resaved, warnings) = parse(&fixture("go2rtc_resaved.yaml")).unwrap();
    // go2rtc saves the preloads it holds under a key of its own: not ours to
    // follow (openIVR rewrites the whole file at every start), said, ignored.
    assert!(warnings.iter().all(|w| w.contains("preload")), "{warnings:?}");
    assert!(!warnings.is_empty(), "the `preload:` section is reported, not silently dropped");
    // Block-style lists, sorted keys: the same sources for every stream the
    // two files share (the copy go2rtc kept has no camera credential in it,
    // and none of the substream that camera declares).
    let mut shared = 0;
    for (name, other) in &resaved.streams {
        let Some((_, sources)) = written.streams.iter().find(|(n, _)| n == name) else { continue };
        let bare: Vec<String> = sources.iter().map(|s| s.replace("admin:s3cret@", "")).collect();
        assert_eq!(other, &bare, "{name}");
        shared += 1;
    }
    assert!(shared >= 15, "{shared} streams compared");
    assert!(names(&resaved).contains(&"camera_11_src"), "a camera the other file does not have");
}

/// Starts the server on what openIVR wrote, only the listen addresses moved
/// to free ports, the cameras' addresses to a closed loopback one (every
/// source is on demand: nothing connects, and nothing could reach a camera).
async fn start_on(file: &str) -> (App, Config) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let rtsp_port = probe.local_addr().unwrap().port();
    drop(probe);
    let text = fixture(file)
        .replace("127.0.0.1:1984", "127.0.0.1:0")
        .replace("127.0.0.1:8554", &format!("127.0.0.1:{rtsp_port}"))
        .replace(":8555", "127.0.0.1:0")
        .replace("192.0.2.49", "127.0.0.1")
        .replace("192.0.2.52", "127.0.0.1")
        .replace("192.0.2.73", "127.0.0.1")
        .replace("[192.0.2.41]", "[127.0.0.1]");
    let (config, _) = parse(&text).unwrap();
    (App::start(&config, None).await.unwrap(), config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_starts_on_openivrs_file_and_serves_every_stream_it_can() {
    let (app, config) = start_on("openivr_written.yaml").await;
    let mut served: Vec<String> = app.hub().streams().iter().map(|s| s.name().to_owned()).collect();
    served.sort();
    // The webcams are `http(s)://` sources (R1): refused one by one, the
    // other streams of the file, the cameras' public streams among them,
    // stay up. When R1 lands this list is empty and the test says so.
    let webcams = ["camera_4_src", "camera_5_src"];
    let mut expected: Vec<String> =
        config.streams.iter().map(|(n, _)| n.clone()).filter(|n| !webcams.contains(&n.as_str())).collect();
    expected.sort();
    assert_eq!(served, expected, "everything but the webcams' own sources");
    assert!(app.shutdown().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_with_relay_servers_starts_and_stops_in_time() {
    let (app, config) = start_on("openivr_written_ice.yaml").await;
    assert_eq!(config.webrtc.ice_servers.len(), 1);
    assert!(app.hub().get("camera_1").is_some());
    assert!(app.shutdown().await);
}
