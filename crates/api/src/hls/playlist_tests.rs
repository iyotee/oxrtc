use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;

use super::*;
use crate::hls::window::Settings;

fn window(gop: u64, seconds: u64) -> Window {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    for n in 0..seconds * 25 {
        let at = Duration::from_millis(n * 40);
        window.push(
            vec![Bytes::from_static(&[0; 4])],
            true,
            n % gop == 0,
            at,
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + at,
        );
    }
    window
}

#[test]
fn rfc3339_in_utc_to_the_millisecond() {
    assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_millis(1_700_000_000_250)), "2023-11-14T22:13:20.250Z");
    assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
    assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(4_102_444_799)), "2099-12-31T23:59:59.000Z");
}

#[test]
fn the_master_playlist_is_go2rtcs() {
    let text = master("abc", &["avc1.64001f".to_owned(), "flac".to_owned()]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "#EXTM3U");
    assert!(lines.contains(&"#EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"avc1.64001f,fLaC\""));
    assert_eq!(lines.last(), Some(&"hls/playlist.m3u8?id=abc"));
}

#[test]
fn a_low_latency_fmp4_playlist() {
    let w = window(15, 5);
    let text = View { id: "s1", kind: Kind::Fmp4, window: &w, low_latency: true }.media();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(&lines[..3], &["#EXTM3U", "#EXT-X-VERSION:9", "#EXT-X-TARGETDURATION:2"]);
    assert!(lines.contains(&"#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=0.400,HOLD-BACK=6.000"));
    assert!(lines.contains(&"#EXT-X-PART-INF:PART-TARGET=0.200"));
    assert!(lines.contains(&"#EXT-X-MEDIA-SEQUENCE:0"));
    assert_eq!(lines.iter().filter(|l| l.starts_with("#EXT-X-MAP:URI=\"init.mp4?id=s1&i=1\"")).count(), 1);
    let extinf: Vec<&str> = lines.iter().filter(|l| l.starts_with("#EXTINF:")).copied().collect();
    assert!(extinf.iter().all(|l| *l == "#EXTINF:1.200,"), "{extinf:?}");
    let segments: Vec<&str> = lines.iter().filter(|l| l.starts_with("segment.m4s")).copied().collect();
    assert_eq!(segments.first(), Some(&"segment.m4s?id=s1&n=0"));
    assert_eq!(lines.iter().filter(|l| l.starts_with("#EXT-X-PROGRAM-DATE-TIME:")).count(), segments.len() + 1);
    assert!(lines.contains(&"#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20.000Z"));
    // Parts of the two newest complete segments and of the open one only.
    let parted: std::collections::BTreeSet<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("#EXT-X-PART:"))
        .filter_map(|l| l.split("&n=").nth(1))
        .filter_map(|l| l.split('&').next())
        .collect();
    let open = w.open().unwrap().msn;
    let expected: std::collections::BTreeSet<String> = [open - 2, open - 1, open].iter().map(u64::to_string).collect();
    assert_eq!(parted, expected.iter().map(String::as_str).collect());
    assert!(lines.iter().any(|l| l.ends_with(",INDEPENDENT=YES")));
    let (msn, part) = w.next_part().unwrap();
    assert_eq!(
        lines.last().unwrap(),
        &format!("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part.m4s?id=s1&n={msn}&p={part}\"")
    );
}

#[test]
fn a_ts_playlist_without_low_latency() {
    let w = window(160, 20);
    let text = View { id: "t", kind: Kind::Ts, window: &w, low_latency: false }.media();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(&lines[..3], &["#EXTM3U", "#EXT-X-VERSION:3", "#EXT-X-TARGETDURATION:7"]);
    assert!(!text.contains("EXT-X-PART") && !text.contains("EXT-X-MAP") && !text.contains("PRELOAD"));
    assert_eq!(lines.iter().filter(|l| **l == "#EXTINF:6.400,").count(), 3);
    assert!(lines.contains(&"segment.ts?id=t&n=2"));
    assert!(!text.contains("n=3"), "the open segment is not listed");
}

#[test]
fn a_dash_manifest_over_the_same_segments() {
    let w = window(25, 5);
    let zero = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let text = mpd("d", &w, &["avc1.64001f".to_owned()], zero, zero + Duration::from_secs(5), MpdAt::Session);
    assert!(text.contains("<Location>manifest.mpd?id=d</Location>") && !text.contains("BaseURL"));
    let api = mpd("d", &w, &["avc1.64001f".to_owned()], zero, zero, MpdAt::Api);
    assert!(api.contains("<BaseURL>hls/</BaseURL>\n  <Location>hls/manifest.mpd?id=d</Location>"), "{api}");
    assert!(text.contains("type=\"dynamic\""));
    assert!(text.contains("availabilityStartTime=\"2023-11-14T22:13:20.000Z\""));
    assert!(text.contains("media=\"segment.m4s?id=d&amp;n=$Number$\" startNumber=\"0\""));
    assert!(text.contains("initialization=\"init.mp4?id=d&amp;i=1\""));
    assert!(text.contains("mimeType=\"video/mp4\"") && text.contains("codecs=\"avc1.64001f\""));
    assert_eq!(text.matches("<S t=").count(), w.segments().count());
    assert!(text.contains("<S t=\"1000\" d=\"1000\"/>"));
}
