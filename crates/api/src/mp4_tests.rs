#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::Duration;

use oxrtc_core::{Codec, MediaWant};

use super::{MAX_FILENAME_CHARS, disposition, duration, frame_wish, stream_wish};

fn one_of(codecs: &[Codec]) -> MediaWant {
    MediaWant::OneOf(codecs.to_vec())
}

const VIDEO: [Codec; 2] = [Codec::H265, Codec::H264];

#[test]
fn no_mp4_and_no_filter_is_go2rtcs_default_h264_or_h265_and_aac() {
    for query in ["", "src=cam", "src=cam&duration=5&filename=a.mp4"] {
        let wish = stream_wish(query).unwrap();
        assert_eq!(wish.want.video, one_of(&VIDEO), "{query}: the camera's own H.265 first, as every egress");
        assert_eq!(wish.want.audio, one_of(&[Codec::Aac]), "{query}");
    }
}

#[test]
fn mp4_flac_and_all_add_the_pcm_family_then_opus() {
    let pcm = [Codec::Pcma, Codec::Pcmu, Codec::L16];
    assert_eq!(stream_wish("src=cam&mp4").unwrap().want.audio, one_of(&[Codec::Aac]), "go2rtc's legacy `mp4`");
    assert_eq!(stream_wish("src=cam&mp4=").unwrap().want.audio, one_of(&[Codec::Aac]));
    let flac = stream_wish("src=cam&mp4=flac").unwrap().want;
    assert_eq!(flac.video, one_of(&VIDEO));
    assert_eq!(flac.audio, one_of(&[[Codec::Aac].as_slice(), &pcm].concat()));
    for mode in ["all", "anything"] {
        let all = stream_wish(&format!("src=cam&mp4={mode}")).unwrap().want;
        assert_eq!(all.audio, one_of(&[[Codec::Opus, Codec::Aac].as_slice(), &pcm].concat()), "{mode}");
    }
    // go2rtc reads the first value only.
    assert_eq!(stream_wish("mp4=flac&mp4=all").unwrap().want.audio, one_of(&[[Codec::Aac].as_slice(), &pcm].concat()));
    // `mp4` decides, whatever the filters say.
    assert_eq!(stream_wish("mp4&video=h264").unwrap().want.video, one_of(&VIDEO));
}

#[test]
fn without_mp4_the_filters_choose_among_what_the_writer_takes() {
    let w = stream_wish("src=cam&video").unwrap().want;
    assert_eq!((w.video, w.audio), (one_of(&VIDEO), MediaWant::Skip), "`video` alone: no sound");
    let w = stream_wish("src=cam&audio").unwrap().want;
    assert_eq!(w.video, MediaWant::Skip);
    assert_eq!(
        w.audio,
        one_of(&[Codec::Opus, Codec::Aac, Codec::Pcma, Codec::Pcmu, Codec::L16]),
        "any sound it writes"
    );
    let w = stream_wish("video=h264&audio=pcma").unwrap().want;
    assert_eq!((w.video, w.audio), (one_of(&[Codec::H264]), one_of(&[Codec::Pcma])));
    let w = stream_wish("video=jpeg").unwrap().want;
    assert!(w.is_empty(), "Motion JPEG is not written: nothing asked, answered `codecs not matched`");
    assert!(stream_wish("video=vp9").is_err(), "an unknown codec is refused");
    assert!(stream_wish("video=opus").is_err(), "a codec of the other kind is refused");
}

#[test]
fn frame_mp4_wants_the_picture_alone() {
    let w = frame_wish().want;
    assert_eq!((w.video, w.audio), (one_of(&VIDEO), MediaWant::Skip));
}

#[test]
fn duration_is_whole_seconds_and_zero_is_none() {
    assert_eq!(duration(None), Ok(None));
    assert_eq!(duration(Some("0")), Ok(None), "go2rtc: 0 is no duration");
    assert_eq!(duration(Some("5")), Ok(Some(Duration::from_secs(5))));
    assert_eq!(duration(Some("4294967295")), Ok(Some(Duration::from_secs(u64::from(u32::MAX)))));
    for bad in ["-1", "5.5", "five", "4294967296", " 5"] {
        assert!(duration(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn a_filename_cannot_break_out_of_its_header() {
    let header = |name: &str| disposition(name).to_str().unwrap().to_owned();
    assert_eq!(header("record.mp4"), "attachment; filename=\"record.mp4\"", "go2rtc's form");
    assert_eq!(header("my clip.mp4"), "attachment; filename=\"my clip.mp4\"");
    assert_eq!(header("a\"b.mp4"), "attachment; filename=\"a_b.mp4\"", "a quote cannot end the value");
    assert_eq!(header("a\r\nSet-Cookie: x.mp4"), "attachment; filename=\"aSet-Cookie: x.mp4\"", "no header injected");
    assert_eq!(header("../../etc/passwd"), "attachment; filename=\"....etcpasswd\"", "no path");
    assert_eq!(header("c:\\x.mp4"), "attachment; filename=\"c:x.mp4\"");
    assert_eq!(
        header("pi π 1.mp4"),
        "attachment; filename=\"pi _ 1.mp4\"; filename*=UTF-8''pi%20%CF%80%201.mp4",
        "RFC 6266 for a name that is not ASCII"
    );
    let long = "x".repeat(10_000);
    assert_eq!(header(&long).matches('x').count(), MAX_FILENAME_CHARS);
}
