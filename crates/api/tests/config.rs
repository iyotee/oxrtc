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
    // go2rtc's `api.origin` is read (`/api/ws`, D39), not warned about.
    assert_eq!(cfg.api.origin.as_deref(), Some("*"));
    assert_eq!(cfg.log.level, "debug");
    let all = warnings.join("\n");
    for key in ["srtp", "webrtc.filters", "webrtc.ice_servers[0].password", "log.format", "rtsp.default_query"] {
        assert!(all.contains(key), "{key} not warned: {all}");
    }
    assert!(!all.contains("api.origin"), "{all}");
    assert_eq!(warnings.len(), 5);
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

/// W5: `ffmpeg.native_audio`, a switch, not a template named so.
#[test]
fn native_audio_is_a_switch() {
    let read = |text: &str| config::parse(text).map(|(cfg, _)| cfg.ffmpeg);
    let off = read(
        "ffmpeg:
  native_audio: false
",
    )
    .unwrap();
    assert_eq!(off.native_audio, Some(false));
    assert!(off.templates.is_empty(), "{:?}", off.templates);
    assert_eq!(
        read(
            "ffmpeg:
  native_audio: true
"
        )
        .unwrap()
        .native_audio,
        Some(true)
    );
    assert_eq!(
        read(
            "ffmpeg:
  native_audio:
"
        )
        .unwrap()
        .native_audio,
        None
    );
    assert_eq!(
        read(
            "ffmpeg:
  bin: ffmpeg
"
        )
        .unwrap()
        .native_audio,
        None
    );
    let error = read(
        "ffmpeg:
  native_audio: maybe
",
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::Invalid(ref why) if why.contains("ffmpeg.native_audio")), "{error}");
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
rtsp:
  username: rtspuser
  password: rtsp-pass-7
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
    let shown = format!("{cfg:?} {cfg:#?} {:?} {:?}", cfg.api, cfg.rtsp);
    // The TURN user name is half of the TURN credential (openIVR's names
    // the site it bills), unlike the API's.
    for secret in [
        "api-pass-1",
        "api-token-2",
        "rtsp-pass-7",
        "turnuser",
        "turn-cred-3",
        "cam-pass-4",
        "cam-pass-5",
        "cam-pass-6",
    ] {
        assert!(!shown.contains(secret), "Debug leaks {secret}: {shown}");
    }
    assert!(shown.contains("admin") && shown.contains("192.168.1.100"));
    assert!(shown.contains("fixed_input") && shown.contains("192.168.1.101"), "{shown}");
    let auth = Auth::from_config(&cfg.api).unwrap();
    assert!(!format!("{auth:?}").contains("api-pass-1") && !format!("{auth:?}").contains("api-token-2"));
}

#[test]
fn rtsp_credentials_are_read_as_a_pair_and_their_misuse_is_refused_without_echoing_them() {
    let (cfg, warnings) = config::parse(
        "rtsp:
  listen: ':8554'
  username: operator
  password: 'p:w d'
",
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!((cfg.rtsp.username.as_deref(), cfg.rtsp.password.as_deref()), (Some("operator"), Some("p:w d")));
    assert!(cfg.rtsp.local_auth, "loopback authenticates unless the file says otherwise");
    // A half credential would protect nothing, or lock every reader out.
    for (key, other) in [("username", "password"), ("password", "username")] {
        let text = format!(
            "rtsp:
  {key}: secret-value
"
        );
        let shown = config::parse(&text).unwrap_err().to_string();
        assert!(shown.contains(&format!("rtsp.{key} is set without rtsp.{other}")), "{shown}");
        assert!(!shown.contains("secret-value"), "the value is not echoed: {shown}");
    }
    let shown = config::parse(
        "rtsp:
  username: 'a:b'
  password: secret-value
",
    )
    .unwrap_err()
    .to_string();
    assert!(shown.contains("':'") && !shown.contains("secret-value"), "{shown}");
    let long = "x".repeat(config::MAX_RTSP_CREDENTIAL_BYTES + 1);
    let shown = config::parse(&format!(
        "rtsp:
  username: u
  password: {long}
"
    ))
    .unwrap_err()
    .to_string();
    assert!(shown.contains("at most 256 bytes") && !shown.contains(&long), "{shown}");
    assert!(
        config::parse(&format!(
            "rtsp:
  username: u
  password: {}
",
            "x".repeat(256)
        ))
        .is_ok()
    );
    // Empty is no protection: accepted, as on go2rtc.
    let (cfg, _) = config::parse(
        "rtsp:
  username: ''
  password: ''
",
    )
    .unwrap();
    assert_eq!((cfg.rtsp.username, cfg.rtsp.password), (None, None));
}

#[test]
fn rtsp_local_auth_is_oxrtcs_and_defaults_to_authenticating_loopback() {
    for (text, expected) in [
        (
            "rtsp:
  local_auth: false
",
            false,
        ),
        (
            "rtsp:
  local_auth: true
",
            true,
        ),
        (
            "rtsp:
  local_auth:
",
            true,
        ),
    ] {
        let (cfg, warnings) = config::parse(text).unwrap();
        assert_eq!(cfg.rtsp.local_auth, expected, "{text}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
    assert!(matches!(
        config::parse(
            "rtsp:
  local_auth: 'no'
"
        ),
        Err(config::ConfigError::Invalid(_))
    ));
}

#[test]
fn local_auth_true_is_what_oxrtc_does_and_false_is_not_followed() {
    // openIVR writes api.local_auth: true, which oxrtc does anyway (D7):
    // accepted without a word. Loopback callers authenticate either way
    // (`loopback_is_authenticated_too`).
    let (_, warnings) = config::parse("api:\n  local_auth: true\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let (_, warnings) = config::parse("api:\n  local_auth: false\n").unwrap();
    assert!(warnings.iter().any(|w| w.contains("local_auth: false is not followed")), "{warnings:?}");
    assert!(matches!(config::parse("api:\n  local_auth: 'yes'\n"), Err(config::ConfigError::Invalid(_))));
}

#[test]
fn the_api_permissions_are_read_from_the_file_and_default_to_go2rtcs_stream_changes_only() {
    let (cfg, warnings) = config::parse("api:\n  token: t\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let p = cfg.api.permissions;
    assert!(p.stream_changes && !p.config_read && !p.config_write && !p.restart && !p.exit, "{p:?}");
    let text = "api:\n  token: t\n  allow_stream_changes: false\n  allow_config_read: true\n  allow_config_write: true\n  allow_restart: true\n  allow_exit: true\n";
    let (cfg, warnings) = config::parse(text).unwrap();
    assert!(warnings.is_empty(), "every key is known: {warnings:?}");
    let p = cfg.api.permissions;
    assert!(!p.stream_changes && p.config_read && p.config_write && p.restart && p.exit, "{p:?}");
    // The write implies the read (an editor reads what it writes).
    let (cfg, _) = config::parse("api:\n  token: t\n  allow_config_write: true\n").unwrap();
    assert!(cfg.api.permissions.reads_config());
    // An empty value keeps the default; anything but a boolean is refused, by its key.
    let (cfg, _) = config::parse("api:\n  token: t\n  allow_stream_changes:\n").unwrap();
    assert!(cfg.api.permissions.stream_changes);
    for key in config::PERMISSION_KEYS {
        let error = config::parse(&format!("api:\n  token: t\n  {key}: \"yes\"\n")).unwrap_err();
        assert!(error.to_string().contains(&format!("api.{key}")), "{error}");
    }
}
