//! Tests of the SDP a `DESCRIBE` answers and an `ANNOUNCE` is read from.

use bytes::Bytes;

use super::*;

fn text(tracks: &[TrackInfo]) -> String {
    String::from_utf8(describe(tracks, None, None, None)).unwrap()
}

#[test]
fn h264_with_parameter_sets() {
    let sps = Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f, 0xac]);
    let pps = Bytes::from_static(&[0x68, 0xee, 0x3c, 0xb0]);
    let sdp = text(&[TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps, pps })]);
    assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"), "{sdp}");
    assert!(
        sdp.contains("a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z2QAH6w=,aO48sA==;profile-level-id=64001F"),
        "{sdp}"
    );
    assert!(sdp.contains("a=control:trackID=0\r\n"), "{sdp}");
}

#[test]
fn audio_codecs() {
    let sdp = text(&[
        TrackInfo::video(Codec::H265),
        TrackInfo::audio(Codec::Pcma, 8000, 1),
        TrackInfo::audio(Codec::Pcmu, 16_000, 1),
        TrackInfo::audio(Codec::Opus, 48_000, 2),
        TrackInfo::audio(Codec::L16, 44_100, 2),
        TrackInfo::audio(Codec::Aac, 16_000, 1)
            .with_params(CodecParams::Aac { config: Bytes::from_static(&[0x14, 0x08]) }),
    ]);
    assert!(sdp.contains("a=rtpmap:96 H265/90000"), "{sdp}");
    assert!(sdp.contains("m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:98 PCMU/16000"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:99 opus/48000/2"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:100 L16/44100/2"), "{sdp}");
    assert!(sdp.contains("a=rtpmap:101 mpeg4-generic/16000/1"), "{sdp}");
    assert!(sdp.contains("mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1408"), "{sdp}");
    assert!(sdp.contains("a=control:trackID=5"), "{sdp}");
    // Video first, as given.
    assert!(sdp.find("m=video") < sdp.find("m=audio"));
}

#[test]
fn jpeg_is_described_on_its_static_payload_type() {
    let sdp = text(&[TrackInfo::video(Codec::Jpeg), TrackInfo::audio(Codec::Pcma, 8000, 1)]);
    let video = "m=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\na=control:trackID=0\r\n";
    assert!(sdp.contains(video), "{sdp}");
    assert!(!sdp.contains("fmtp:26"), "{sdp}");
    assert!(sdp.contains("a=control:trackID=1"), "{sdp}");
}

#[test]
fn a_webcam_is_announced_at_its_rate_however_fast() {
    let jpeg = [TrackInfo::video(Codec::Jpeg), TrackInfo::audio(Codec::Pcma, 8000, 1)];
    let one = Some(Duration::from_secs(1));
    let one_a_second = String::from_utf8(describe(&jpeg, one, one, None)).unwrap();
    let video = "m=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\na=framerate:1.000\r\na=control:trackID=0\r\n";
    assert!(one_a_second.contains(video), "{one_a_second}");
    assert_eq!(one_a_second.matches("a=framerate").count(), 1, "the sound has none: {one_a_second}");
    let eight = String::from_utf8(describe(&jpeg, Some(Duration::from_millis(120)), None, None)).unwrap();
    assert!(eight.contains("a=framerate:8.333\r\n"), "{eight}");
    // The owner, 2026-11-20: a fast webcam on no grid is told its rate
    // too - the mean of its gaps, not their median (37.8 ms on an uneven
    // minute of his 30-a-second webcam). Nothing was said before.
    let (median, thirty) = (Some(Duration::from_micros(37_800)), Some(Duration::from_secs_f64(1.0 / 30.0)));
    let fast = String::from_utf8(describe(&jpeg, median, thirty, None)).unwrap();
    assert!(fast.contains("a=framerate:30.000\r\n"), "{fast}");
    let only_the_pace = String::from_utf8(describe(&jpeg, Some(Duration::from_millis(40)), None, None)).unwrap();
    assert!(only_the_pace.contains("a=framerate:25.000\r\n"), "{only_the_pace}");
    // A coded track keeps its pace: its dates are a clock's, not arrivals.
    let h264 = [TrackInfo::video(Codec::H264)];
    let coded = String::from_utf8(describe(&h264, Some(Duration::from_millis(40)), thirty, None)).unwrap();
    assert!(coded.contains("a=framerate:25.000\r\n"), "{coded}");
    assert!(!text(&jpeg).contains("framerate"), "nothing known, nothing said");
}

#[test]
fn a_coded_tracks_pace_is_announced_whatever_it_is() {
    // 70 ms: what the owner's H.265 cameras send, and do not say in their
    // parameter sets.
    let tracks = [TrackInfo::video(Codec::H265), TrackInfo::audio(Codec::Pcma, 8000, 1)];
    let sdp = String::from_utf8(describe(&tracks, Some(Duration::from_millis(70)), None, None)).unwrap();
    assert!(sdp.contains("a=framerate:14.286\r\na=control:trackID=0\r\n"), "{sdp}");
    assert_eq!(sdp.matches("a=framerate").count(), 1, "the sound has none: {sdp}");
    for (interval, said) in [(40, "25.000"), (1000, "1.000"), (33, "30.303")] {
        let h264 = [TrackInfo::video(Codec::H264)];
        let sdp = String::from_utf8(describe(&h264, Some(Duration::from_millis(interval)), None, None)).unwrap();
        assert!(sdp.contains(&format!("a=framerate:{said}\r\n")), "{interval} ms: {sdp}");
    }
    assert!(!text(&tracks).contains("framerate"), "no pace known, none said");
    // A pace that would read as none at all is not said.
    for never in [Duration::ZERO, Duration::from_secs(4000), Duration::MAX] {
        let jpeg = [TrackInfo::video(Codec::Jpeg)];
        for tracks in [&tracks[..1], &jpeg[..]] {
            let sdp = String::from_utf8(describe(tracks, Some(never), Some(never), None)).unwrap();
            assert!(!sdp.contains("framerate"), "{never:?}: {sdp}");
        }
    }
}

#[test]
fn a_track_dated_on_a_grid_is_announced_at_the_grid_whatever_fills_it() {
    // The owner's ESP32: 8 pictures a second (120 ms the median), every
    // date a multiple of 40 ms. FFmpeg's guess reads 25.
    let jpeg = [TrackInfo::video(Codec::Jpeg)];
    let (pace, slot) = (Some(Duration::from_millis(120)), Some(Duration::from_millis(40)));
    let sdp = String::from_utf8(describe(&jpeg, pace, pace, slot)).unwrap();
    assert!(sdp.contains("a=framerate:25.000\r\n"), "{sdp}");
    // A Motion JPEG camera with a clock of its own that fills every slot
    // is faster than ten a second and announced all the same: its dates
    // say its rate, where a webcam's arrivals do not.
    let sdp = String::from_utf8(describe(&jpeg, slot, slot, slot)).unwrap();
    assert!(sdp.contains("a=framerate:25.000\r\n"), "{sdp}");
    // A coded camera that sends every other slot of its grid.
    let h265 = [TrackInfo::video(Codec::H265)];
    let (pace, slot) = (Some(Duration::from_millis(140)), Some(Duration::from_millis(70)));
    let sdp = String::from_utf8(describe(&h265, pace, pace, slot)).unwrap();
    assert!(sdp.contains("a=framerate:14.286\r\n"), "{sdp}");
}

#[test]
fn a_published_jpeg_is_refused() {
    // The server sends Motion JPEG and has no depacketizer for it.
    let sdp = describe(&[TrackInfo::video(Codec::Jpeg), TrackInfo::audio(Codec::Pcma, 8000, 1)], None, None, None);
    let announced = parse_announce(&sdp, 8).unwrap();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].info.codec, Codec::Pcma);
}

#[test]
fn our_own_description_reads_back() {
    let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
    let announced = parse_announce(&describe(&tracks, None, None, None), 8).unwrap();
    assert_eq!(announced.len(), 2);
    assert_eq!(announced[0].info, tracks[0]);
    assert_eq!(announced[1].info, tracks[1]);
    assert_eq!(announced[1].control.as_deref(), Some("trackID=1"));
    assert_eq!(announced[1].spec.encoding, "opus");
}

#[test]
fn ffmpeg_announce() {
    let body = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=No Name\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
a=tool:libavformat 62.3.100\r\nm=video 0 RTP/AVP 96\r\nb=AS:200\r\na=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z2QAHqwsaoLA9puCgIKgAAADACAAAAMD0IAA,aO4xshsA; profile-level-id=64001E\r\n\
a=control:streamid=0\r\nm=audio 0 RTP/AVP 8\r\na=control:streamid=1\r\nm=application 0 RTP/AVP 107\r\n";
    let announced = parse_announce(body, 8).unwrap();
    assert_eq!(announced.len(), 2);
    assert_eq!(announced[0].info.codec, Codec::H264);
    assert_eq!(announced[0].control.as_deref(), Some("streamid=0"));
    assert!(announced[0].spec.fmtp.as_deref().unwrap().starts_with("packetization-mode=1"));
    assert_eq!(announced[1].info, TrackInfo::audio(Codec::Pcma, 8000, 1));
    assert_eq!(parse_announce(body, 1).unwrap().len(), 1, "bounded by max_tracks");
    assert!(parse_announce(b"not an sdp", 8).is_err());
}

/// Found by the `sdp_mutated_announce` property: sdp-types 0.1 asserted
/// that the line after a malformed one was an `m=` line, so this body,
/// sent by any local process before its publication token is checked,
/// panicked the connection task.
#[test]
fn a_malformed_line_before_a_session_line_is_refused() {
    assert!(parse_announce(b"v=0\r\nbad\r\nc=IN IP4 0.0.0.0\r\n", 8).is_err());
}
