//! The configuration file: go2rtc's YAML subset as openIVR writes it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;

use oxrtc_api::Auth;
use oxrtc_api::config::{self, ConfigError, IceServer, MAX_CONFIG_BYTES};

/// The example of docs/COMPATIBILITY.md, verbatim.
const COMPAT: &str = r#"api:
  listen: "127.0.0.1:1984"
  username: "..."        # optional in go2rtc; enforced by oxrtc on every call
  password: "..."
rtsp:
  listen: "127.0.0.1:8554"
webrtc:
  listen: ":8555"
  candidates: ["192.168.1.42"]        # extra host candidates, with or without :port
  ice_servers:                        # STUN/TURN for viewers outside the LAN
    - urls: ["turn:relay.example:3478"]
      username: "..."
      credential: "..."
streams:
  camera_1_src: ["rtsp://user:pass@192.168.1.100:554/live/ch00_0#timeout=15"]
  camera_1: [
    "rtsp://127.0.0.1:8554/camera_1_src?video#timeout=20",
    "ffmpeg:camera_1_src#video=h264#width=1920#height=1080",
    "rtsp://127.0.0.1:8554/camera_1_rec?audio=opus",
  ]
ffmpeg:
  bin: /path/to/ffmpeg
  h264: "-codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough"
  dedup_input: "-fflags nobuffer -flags low_delay -probesize 32 -i {input}"
log:
  level: info
"#;

#[test]
fn parses_the_compatibility_example() {
    let (cfg, warnings) = config::parse(COMPAT).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(cfg.api.listen, "127.0.0.1:1984");
    assert_eq!(cfg.api.username.as_deref(), Some("..."));
    assert_eq!(cfg.api.password.as_deref(), Some("..."));
    assert!(!cfg.api.allow_unauthenticated);
    assert_eq!(cfg.rtsp.listen, "127.0.0.1:8554");
    assert_eq!(cfg.webrtc.listen, ":8555");
    assert_eq!(cfg.webrtc.candidates, vec!["192.168.1.42"]);
    assert_eq!(
        cfg.webrtc.ice_servers,
        vec![IceServer {
            urls: vec!["turn:relay.example:3478".into()],
            username: Some("...".into()),
            credential: Some("...".into()),
        }]
    );
    let names: Vec<&str> = cfg.streams.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["camera_1_src", "camera_1"], "file order is kept");
    assert_eq!(cfg.streams[0].1, ["rtsp://user:pass@192.168.1.100:554/live/ch00_0#timeout=15"]);
    assert_eq!(
        cfg.streams[1].1,
        [
            "rtsp://127.0.0.1:8554/camera_1_src?video#timeout=20",
            "ffmpeg:camera_1_src#video=h264#width=1920#height=1080",
            "rtsp://127.0.0.1:8554/camera_1_rec?audio=opus",
        ]
    );
    assert_eq!(cfg.ffmpeg.bin, Some(PathBuf::from("/path/to/ffmpeg")));
    assert_eq!(cfg.ffmpeg.templates.len(), 2);
    assert!(cfg.ffmpeg.templates["h264"].starts_with("-codec:v libx264"));
    assert_eq!(cfg.ffmpeg.templates["dedup_input"], "-fflags nobuffer -flags low_delay -probesize 32 -i {input}");
    assert_eq!(cfg.log.level, "info");
    assert!(matches!(Auth::from_config(&cfg.api).unwrap(), Auth::Credentials { basic: Some(_), bearer: None }));
}

#[test]
fn block_style_and_single_strings_parse_the_same() {
    let block = r#"
streams:
  zeta:
    - rtsp://user:pass@192.168.1.100:554/live/ch00_0#timeout=15
  camera_1:
    - "rtsp://127.0.0.1:8554/camera_1_src?video#timeout=20"
    - ffmpeg:camera_1_src#video=h264#width=1920#height=1080
  alpha: rtsp://10.0.0.1/one
webrtc:
  candidates:
    - 192.168.1.42
    - 10.0.0.5:8555
  ice_servers:
    - urls: turn:relay.example:3478
"#;
    let (cfg, warnings) = config::parse(block).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let names: Vec<&str> = cfg.streams.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["zeta", "camera_1", "alpha"]);
    assert_eq!(cfg.streams[1].1.len(), 2);
    assert_eq!(cfg.streams[2].1, ["rtsp://10.0.0.1/one"]);
    assert_eq!(cfg.webrtc.candidates, ["192.168.1.42", "10.0.0.5:8555"]);
    assert_eq!(cfg.webrtc.ice_servers[0].urls, ["turn:relay.example:3478"]);
    assert_eq!(cfg.webrtc.ice_servers[0].credential, None);
}

#[test]
fn defaults_apply_to_an_empty_file() {
    for text in ["", "\n# nothing\n", "api:\nrtsp:\n"] {
        let (cfg, warnings) = config::parse(text).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(cfg.api.listen, "127.0.0.1:1984");
        assert_eq!(cfg.rtsp.listen, "127.0.0.1:8554");
        assert_eq!(cfg.webrtc.listen, ":8555");
        assert_eq!(cfg.log.level, "info");
        assert!(cfg.streams.is_empty() && cfg.ffmpeg.bin.is_none() && cfg.ffmpeg.templates.is_empty());
        assert!(matches!(Auth::from_config(&cfg.api), Err(ConfigError::NoAuth)));
    }
    let (cfg, _) = config::parse("api:\n  allow_unauthenticated: true\n  token: ''\n").unwrap();
    assert_eq!(Auth::from_config(&cfg.api).unwrap(), Auth::Unauthenticated);
}

#[test]
fn unknown_keys_are_warned_and_ignored() {
    let text = "
api:
  listen: ':1984'
  origin: '*'
  token: tok
srtp:
  listen: ':8443'
webrtc:
  filters: {candidates: [x]}
  ice_servers: [{urls: [stun:x], password: leaked-value}]
log:
  format: color
  level: debug
rtsp:
  default_query: video
";
    let (cfg, warnings) = config::parse(text).unwrap();
    assert_eq!(cfg.api.listen, ":1984");
    assert_eq!(cfg.api.token.as_deref(), Some("tok"));
    assert_eq!(cfg.log.level, "debug");
    let all = warnings.join("\n");
    for key in
        ["api.origin", "srtp", "webrtc.filters", "webrtc.ice_servers[0].password", "log.format", "rtsp.default_query"]
    {
        assert!(all.contains(key), "{key} not warned: {all}");
    }
    assert_eq!(warnings.len(), 6);
    assert!(!all.contains("leaked-value") && !all.contains("color"), "warnings name keys, not values");
}

#[test]
fn a_stream_without_source_is_kept_and_warned() {
    let (cfg, warnings) = config::parse("streams:\n  empty:\n  one: fake:x\n").unwrap();
    assert_eq!(cfg.streams, vec![("empty".to_owned(), vec![]), ("one".to_owned(), vec!["fake:x".to_owned()])]);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("empty"));
}

#[test]
fn wrong_shapes_and_bad_yaml_are_refused_without_echoing_values() {
    for text in [
        "api: [1, 2]",
        "api:\n  allow_unauthenticated: 'yes'",
        "streams:\n  cam: {url: 'rtsp://u:hunter2@x'}",
        "streams:\n  cam: [['rtsp://u:hunter2@x']]",
        "webrtc:\n  ice_servers: turn:x",
        "- just\n- a list",
    ] {
        let error = config::parse(text).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)), "{text}: {error:?}");
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }
    let error = config::parse("api:\n  listen: [unclosed\n").unwrap_err();
    assert!(matches!(error, ConfigError::Yaml(_)), "{error:?}");
    let error = config::parse("streams:\n  a: x\n  a: y\n").unwrap_err();
    assert!(matches!(error, ConfigError::Yaml(_)), "duplicate keys are refused: {error:?}");
}

/// R9: the per-address limits, oxrtc-only keys inside go2rtc's sections.
#[test]
fn per_address_limits_are_read_and_checked() {
    let (cfg, warnings) = config::parse(
        "api:
  max_connections_per_ip: 10
rtsp:
  max_connections_per_ip: 20
webrtc:
  max_sessions: 300
  max_sessions_per_ip: 3
ffmpeg:
  max_processes: 48
  max_memory_mib: 1536
",
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(cfg.webrtc.max_sessions, Some(300));
    assert_eq!(cfg.api.max_connections_per_ip, Some(10));
    assert_eq!(cfg.rtsp.max_connections_per_ip, Some(20));
    assert_eq!(cfg.webrtc.max_sessions_per_ip, Some(3));
    // A limit, not a template named max_processes.
    assert_eq!(cfg.ffmpeg.max_processes, Some(48));
    assert_eq!(cfg.ffmpeg.max_memory_mib, Some(1536));
    assert!(cfg.ffmpeg.templates.is_empty(), "{:?}", cfg.ffmpeg.templates);
    // Absent or empty: the servers' defaults.
    let (cfg, _) = config::parse(
        "rtsp:
  max_connections_per_ip:
",
    )
    .unwrap();
    assert_eq!((cfg.api.max_connections_per_ip, cfg.rtsp.max_connections_per_ip), (None, None));
    assert_eq!((cfg.webrtc.max_sessions, cfg.webrtc.max_sessions_per_ip), (None, None));
    assert_eq!((cfg.ffmpeg.max_processes, cfg.ffmpeg.max_memory_mib), (None, None));
    for bad in [
        "api:
  max_connections_per_ip: 0
",
        "rtsp:
  max_connections_per_ip: -4
",
        "webrtc:
  max_sessions_per_ip: 2.5
",
        "webrtc:
  max_sessions_per_ip: lots
",
        "webrtc:
  max_sessions: 0
",
        "ffmpeg:
  max_processes: 0
",
        "ffmpeg:
  max_memory_mib: 0
",
    ] {
        let error = config::parse(bad).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(ref why) if why.contains("at least 1")), "{bad}: {error}");
    }
}

#[test]
fn oversize_is_refused() {
    let big = format!("log:\n  level: info\n# {}\n", "x".repeat(MAX_CONFIG_BYTES));
    assert!(matches!(config::parse(&big), Err(ConfigError::TooLarge { .. })));
    let just_fits = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES - 3));
    assert_eq!(just_fits.len(), MAX_CONFIG_BYTES);
    assert!(config::parse(&just_fits).is_ok());

    let dir = std::env::temp_dir().join(format!("oxrtc-api-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.yaml");
    std::fs::write(&path, &big).unwrap();
    assert!(matches!(config::load(&path), Err(ConfigError::TooLarge { .. })));
    let path_ok = dir.join("ok.yaml");
    std::fs::write(&path_ok, COMPAT).unwrap();
    assert_eq!(config::load(&path_ok).unwrap().streams.len(), 2);
    assert!(matches!(config::load(&dir.join("missing.yaml")), Err(ConfigError::Io { .. })));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn debug_hides_every_secret() {
    let text = "
api:
  username: admin
  password: api-pass-1
  token: api-token-2
webrtc:
  ice_servers:
    - urls: ['turn:relay.example:3478']
      username: turnuser
      credential: turn-cred-3
streams:
  cam: ['rtsp://user:cam-pass-4@192.168.1.100/live', 'http://cam/snap?password=cam-pass-5']
ffmpeg:
  fixed_input: '-rtsp_transport tcp -i rtsp://user:cam-pass-6@192.168.1.101/live'
";
    let (cfg, _) = config::parse(text).unwrap();
    let shown = format!("{cfg:?} {cfg:#?} {:?}", cfg.api);
    // The TURN user name is half of the TURN credential (openIVR's names
    // the site it bills), unlike the API's.
    for secret in ["api-pass-1", "api-token-2", "turnuser", "turn-cred-3", "cam-pass-4", "cam-pass-5", "cam-pass-6"] {
        assert!(!shown.contains(secret), "Debug leaks {secret}: {shown}");
    }
    assert!(shown.contains("admin") && shown.contains("192.168.1.100"));
    assert!(shown.contains("fixed_input") && shown.contains("192.168.1.101"), "{shown}");
    let auth = Auth::from_config(&cfg.api).unwrap();
    assert!(!format!("{auth:?}").contains("api-pass-1") && !format!("{auth:?}").contains("api-token-2"));
}

#[test]
fn rtsp_credentials_are_refused_and_local_auth_is_followed() {
    // go2rtc protects its RTSP server with these; oxrtc has no RTSP
    // authentication yet, and must not serve such a file open.
    for key in ["username", "password"] {
        let text = format!("rtsp:\n  listen: ':8554'\n  {key}: secret-value\n");
        let error = config::parse(&text).unwrap_err();
        let shown = error.to_string();
        assert!(shown.contains(&format!("rtsp.{key}")) && shown.contains("not implemented"), "{shown}");
        assert!(!shown.contains("secret-value"), "the value is not echoed: {shown}");
    }
    // Empty is no protection: accepted.
    assert!(config::parse("rtsp:\n  username: ''\n").is_ok());
    // openIVR writes api.local_auth: true, which oxrtc does anyway (D7).
    let (_, warnings) = config::parse("api:\n  local_auth: true\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (_, warnings) = config::parse("api:\n  local_auth: false\n").unwrap();
    assert!(warnings.iter().any(|w| w.contains("local_auth: false is not followed")), "{warnings:?}");
}
