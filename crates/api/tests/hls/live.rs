//! HLS and DASH as a camera in real time makes them (F6P): a camera that
//! stalls, dash.js's delay and first manifest, hls.js's first Low-Latency
//! playlist. A module of `hls.rs`, its setup and requests.

use std::time::{Duration, Instant};

use oxrtc_api::HlsLimits;

use super::{get, setup};

/// A camera that stalls: every playlist keeps `PART-TARGET` and
/// `PART-HOLD-BACK`, no part outlasts the target, and the playlist moves on
/// after the stall (F6P). Before F6P the part being written when it stalled
/// was closed only by the frame after the gap, as long as the gap: `PART-TARGET=3.040`,
/// `PART-HOLD-BACK=6.080`, a player holding six seconds back from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_that_stalls_keeps_its_part_target_and_hold_back() {
    for form in ["mp4", "ts"] {
        let (router, _hub) = setup(HlsLimits::default());
        let query = if form == "mp4" { "src=stalls&mp4" } else { "src=stalls" };
        let master = get(&router, &format!("/api/stream.m3u8?{query}"), true).await.text();
        let media = format!("/api/{}", master.lines().last().unwrap());
        let started = Instant::now();
        let mut seen = Vec::new();
        while started.elapsed() < Duration::from_secs(8) {
            seen.push(get(&router, &media, false).await.text());
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        for text in &seen {
            assert!(text.contains("#EXT-X-PART-INF:PART-TARGET=0.100\n"), "{form}: {text}");
            assert!(text.contains("PART-HOLD-BACK=0.200,"), "{form}: {text}");
            for part in text.lines().filter_map(|l| l.strip_prefix("#EXT-X-PART:DURATION=")) {
                let duration: f64 = part.split(',').next().unwrap().parse().unwrap();
                assert!(duration <= 0.1, "{form}: a part longer than its target: {part}");
            }
            for extinf in text.lines().filter_map(|l| l.strip_prefix("#EXTINF:")) {
                let duration: f64 = extinf.trim_end_matches(',').parse().unwrap();
                assert!(duration < 1.5, "{form}: the gap is in no segment: {extinf}");
            }
        }
        let sequence = |text: &str| -> u64 {
            text.lines().find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).unwrap().parse().unwrap()
        };
        // 2.4 s of pictures, 3 s of nothing, 2.6 s more in the test's 8 s:
        // the segments to 6.0 s and the one after the stall are complete.
        let last = seen.last().unwrap();
        assert!(last.matches("#EXTINF:").count() + sequence(last) as usize >= 4, "{form}: it moved on: {last}");
    }
}

/// dash.js plays its live delay behind the live edge, never before the
/// window's start (F6P): told one and a half segments, and given a first
/// manifest of one segment, it reached the end of its buffer once a segment,
/// a picture dropped each time. A new session's first manifest lists more
/// than a segment and the margin, and the delay is two segments and it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dash_player_is_told_a_delay_its_first_manifest_holds() {
    let (router, _hub) = setup(HlsLimits::default());
    let text = get(&router, "/api/stream.mpd?src=live", true).await.text();
    assert!(text.contains("suggestedPresentationDelay=\"PT2.500S\""), "{text}");
    let listed: u64 = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("<S t=\""))
        .map(|l| l.split("d=\"").nth(1).unwrap().split('"').next().unwrap().parse::<u64>().unwrap())
        .sum::<u64>();
    assert!(listed >= 1500, "the first manifest lists {listed} ms of media: {text}");
}

/// hls.js starts `PART-HOLD-BACK` behind the newest part, and from the
/// keyframe before: a first playlist answered as the first segment completed
/// made it start in that segment and read it whole first, 0.4 s more behind
/// the camera with 6.4 s groups for as long as it played (F6P). The first
/// playlist lists, after its complete segment, the parts of the segment being
/// written up to `PART-HOLD-BACK`, so that hls.js starts at that segment's
/// keyframe.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_low_latency_playlist_holds_its_hold_back_past_a_keyframe() {
    for form in ["mp4", "ts"] {
        let (router, _hub) = setup(HlsLimits::default());
        let query = if form == "mp4" { "src=live&mp4" } else { "src=live" };
        let master = get(&router, &format!("/api/stream.m3u8?{query}"), true).await.text();
        let text = get(&router, &format!("/api/{}", master.lines().last().unwrap()), false).await.text();
        let after_segments = text.rsplit_once("#EXTINF:").map(|(_, rest)| rest).unwrap_or_default();
        let open: f64 = after_segments
            .lines()
            .filter_map(|l| l.strip_prefix("#EXT-X-PART:DURATION="))
            .map(|l| l.split(',').next().unwrap().parse::<f64>().unwrap())
            .sum();
        assert!(open >= 0.2, "{form}: the open segment holds {open} s: {text}");
        assert!(after_segments.contains("INDEPENDENT=YES"), "{form}: it starts at a keyframe: {text}");
    }
}

/// A camera nobody read, with 6.4 s groups of pictures (the owner's): its
/// first DASH manifest waits past the 10 s a first HLS playlist waits, for a
/// segment and the margin more than the one being played (F6P) - answered at
/// 10 s with one segment or none, dash.js 5.3.0 showed no picture for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_gop_camera_s_first_dash_manifest_lists_two_segments() {
    let (router, _hub) = setup(HlsLimits::default());
    let text = get(&router, "/api/stream.mpd?src=long", true).await.text();
    let listed = text.lines().filter(|l| l.trim().starts_with("<S t=")).count();
    assert!(listed >= 2, "{text}");
    assert!(text.contains("suggestedPresentationDelay=\"PT13.300S\""), "{text}");
}
