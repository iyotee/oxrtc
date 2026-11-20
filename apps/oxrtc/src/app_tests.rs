use super::*;

#[test]
fn the_per_address_limits_reach_every_listener() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let (configured, _) = oxrtc_api::config::parse(
        "api:
  max_connections_per_ip: 3
rtsp:
  max_connections_per_ip: 4
webrtc:
  max_sessions: 200
  max_sessions_per_ip: 5
",
    )
    .unwrap();
    assert_eq!(serve_limits(&configured).max_connections_per_ip, 3);
    assert_eq!(rtsp_config(&configured, addr).max_connections_per_ip, 4);
    assert_eq!(webrtc_config(&configured, addr).max_sessions_per_ip, 5);
    assert_eq!(webrtc_config(&configured, addr).max_sessions, 200);
    let unset = Config::default();
    assert_eq!(serve_limits(&unset), ServeLimits::default());
    assert_eq!(rtsp_config(&unset, addr).max_connections_per_ip, oxrtc_rtsp_server::DEFAULT_MAX_CONNECTIONS_PER_IP);
    assert_eq!(webrtc_config(&unset, addr).max_sessions_per_ip, oxrtc_webrtc::DEFAULT_MAX_SESSIONS_PER_IP);
    assert_eq!(webrtc_config(&unset, addr).max_sessions, oxrtc_webrtc::DEFAULT_MAX_SESSIONS);
}

#[test]
fn lip_sync_and_the_sound_bound_reach_the_webrtc_server() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let unset = webrtc_config(&Config::default(), addr).playout;
    assert_eq!(unset, oxrtc_webrtc::Playout::default());
    assert!(unset.lip_sync && !unset.pace_sound, "lip sync on, the sound not paced (W1b review)");
    assert_eq!(unset.sound_late_max, Some(oxrtc_webrtc::DEFAULT_SOUND_LATE_MAX));
    let parsed = |text: &str| webrtc_config(&oxrtc_api::config::parse(text).unwrap().0, addr).playout;
    let live = parsed("webrtc:\n  lip_sync: false\n  max_sound_late_ms: 250\n");
    assert!(!live.lip_sync);
    assert_eq!(live.sound_late_max, Some(std::time::Duration::from_millis(250)));
    assert_eq!(parsed("webrtc:\n  max_sound_late_ms: 0\n").sound_late_max, None, "0: all the sound is sent");
}

/// `webrtc.sound_lag_ms` (D30, W1c): absent, the server's default, inside
/// what the owner allowed (20 to 40 ms); `0` dates the sound when it arrived;
/// and the file takes no more than the WebRTC server gives.
#[test]
fn the_sound_lag_reaches_the_webrtc_server() {
    use std::time::Duration;
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let parsed = |text: &str| webrtc_config(&oxrtc_api::config::parse(text).unwrap().0, addr).playout;
    let unset = webrtc_config(&Config::default(), addr).playout.sound_lag;
    assert_eq!(unset, oxrtc_webrtc::DEFAULT_SOUND_LAG);
    assert!((Duration::from_millis(20)..=Duration::from_millis(40)).contains(&unset), "{unset:?}");
    assert_eq!(parsed("webrtc:\n  sound_lag_ms: 0\n").sound_lag, Duration::ZERO, "0: dated when it arrived");
    assert_eq!(parsed("webrtc:\n  sound_lag_ms: 25\n").sound_lag, Duration::from_millis(25));
    assert_eq!(parsed("webrtc:\n  sound_lag_ms:\n").sound_lag, unset, "empty: the default");
    assert_eq!(Duration::from_millis(oxrtc_api::config::MAX_SOUND_LAG_MS), oxrtc_webrtc::MAX_SOUND_LAG);
}

#[test]
fn the_stream_limits_and_queue_budgets_reach_the_hub() {
    let (configured, warnings) = oxrtc_api::config::parse(
        "api:
  max_streams: 40
  max_consumers_per_stream: 12
  max_frame_mib: 16
  gop_cache_mib: 24
  idle_grace_ms: 3000
  transcode_idle_grace_ms: 0
rtsp:
  queue_max_mib: 64
  queue_max_seconds: 20
webrtc:
  queue_max_seconds: 1
",
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let limits = hub_limits(&configured);
    let defaults = Limits::default();
    let expected = Limits {
        max_streams: 40,
        max_consumers_per_stream: 12,
        max_frame_bytes: 16 << 20,
        gop_cache_bytes: 24 << 20,
        idle_grace: Duration::from_secs(3),
        transcode_idle_grace: Duration::ZERO,
        recorder_queue: QueueBudget { max_bytes: 64 << 20, max_age: Duration::from_secs(20) },
        // A part left out keeps its default.
        live_queue: QueueBudget { max_age: Duration::from_secs(1), ..defaults.live_queue },
        ..defaults.clone()
    };
    assert_eq!(limits, expected);
    assert_eq!(hub_limits(&Config::default()), defaults, "a file written for go2rtc keeps every default");
}

#[test]
fn the_ice_servers_reach_the_webrtc_server_as_openivr_writes_them() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    // openIVR's _write_config, once Remote Access is paired.
    let (configured, _) = oxrtc_api::config::parse(
        "webrtc:
  listen: ':8555'
  candidates: [192.168.1.42]
  ice_servers:
  - urls: ['stun:relay.example.org:3478', 'turn:relay.example.org:3478?transport=udp']
    username: '1700000000:site'
    credential: c2VjcmV0
",
    )
    .unwrap();
    let webrtc = webrtc_config(&configured, addr);
    assert_eq!(webrtc.ice_servers.len(), 1);
    assert_eq!(webrtc.ice_servers[0].urls.len(), 2);
    assert_eq!(webrtc.ice_servers[0].username.as_deref(), Some("1700000000:site"));
    assert_eq!(webrtc.ice_servers[0].credential.as_deref(), Some("c2VjcmV0"));
    assert!(webrtc.tcp, "ICE-TCP on the UDP port, as go2rtc");
    assert!(!format!("{webrtc:?}").contains("c2VjcmV0"), "the credential is not printed");
}

/// A transcode's answer is held for no longer than its FFmpeg waits, and
/// how long that is the operator may have written (`ffmpeg.timeout`).
#[test]
fn the_rtsp_server_is_told_how_long_the_transcodes_ffmpeg_waits() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let told = |text: &str| rtsp_config(&oxrtc_api::config::parse(text).unwrap().0, addr).transcode_timeout;
    assert_eq!(told(""), Duration::from_secs(5));
    assert_eq!(told("ffmpeg:\n  bin: ffmpeg\n  timeout: 3\n"), Duration::from_secs(3));
    assert_eq!(told("ffmpeg:\n  timeout: '12'\n"), Duration::from_secs(12));
}

#[test]
fn the_ffmpeg_binary_is_the_configured_one_then_the_environment_then_the_path() {
    let env = || Some(OsString::from("C:/env/ffmpeg.exe"));
    assert_eq!(ffmpeg_bin(Some(Path::new("/opt/ffmpeg")), env()), PathBuf::from("/opt/ffmpeg"));
    assert_eq!(ffmpeg_bin(None, env()), PathBuf::from("C:/env/ffmpeg.exe"));
    assert_eq!(ffmpeg_bin(None, Some(OsString::new())), PathBuf::from("ffmpeg"));
    assert_eq!(ffmpeg_bin(None, None), PathBuf::from("ffmpeg"));
}
