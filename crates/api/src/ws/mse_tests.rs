#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use oxrtc_core::h265_receiver::HevcReceiver;
use oxrtc_core::{Codec, MediaWant, Want};
use oxrtc_mp4::HevcEntry;

use super::{MAX_CODECS, wish};

const PCM: [Codec; 3] = [Codec::Pcma, Codec::Pcmu, Codec::L16];

fn one_of(codecs: &[Codec]) -> MediaWant {
    MediaWant::OneOf(codecs.to_vec())
}

#[test]
fn go2rtcs_player_in_edge_wants_the_camera_s_h265_first_and_every_sound() {
    // What headless Edge 157's `MediaSource.isTypeSupported` keeps of the
    // player's list on this PC.
    let edge = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
    let w = wish(edge);
    assert_eq!(w.want.video, one_of(&[Codec::H265, Codec::H264]), "WebRTC's order: the camera's own H.265 first");
    assert_eq!(w.want.audio, one_of(&[Codec::Opus, Codec::Aac, Codec::Pcma, Codec::Pcmu, Codec::L16]));
    assert_eq!(w.hevc, HevcEntry::Hvc1);
}

#[test]
fn a_browser_without_h265_gets_the_h264_transcode() {
    // Firefox lists no H.265.
    let w = wish("avc1.640029,avc1.64002A,avc1.640033,mp4a.40.2,mp4a.40.5,flac,opus");
    assert_eq!(w.want.video, one_of(&[Codec::H264]));
}

#[test]
fn the_h265_entry_follows_the_list() {
    assert_eq!(wish("hev1.1.6.L153.B0").hevc, HevcEntry::Hev1, "a browser that takes only hev1");
    assert_eq!(wish("hev1.1.6.L153.B0,hvc1.1.6.L153.B0").hevc, HevcEntry::Hvc1, "hvc1 when both: Safari's");
    assert_eq!(wish("HVC1.1.6.L153.B0").want.video, one_of(&[Codec::H265]), "names are read without case");
}

#[test]
fn each_h265_entry_says_what_the_reader_decodes() {
    let main51 = HevcReceiver { space: 0, profile: Some(1), tier: 0, level: Some(153) };
    let edge = "avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus";
    assert_eq!(wish(edge).receivers, Some(vec![main51]), "go2rtc's player: Main at level 5.1");
    let both = wish("hvc1.1.6.L153.B0,hvc1.2.4.L153.B0").receivers.unwrap();
    assert_eq!(both.iter().map(|r| r.profile).collect::<Vec<_>>(), vec![Some(1), Some(2)]);
    // An entry that is not an annex E string names no limit; the family alone neither.
    assert_eq!(wish("hvc1.zz").receivers, Some(vec![HevcReceiver::ANY]));
    assert_eq!(wish("hvc1,avc1.640029").receivers, Some(vec![HevcReceiver::ANY]));
    // A list without H.265: nothing to take; an empty list: go2rtc's default, unchecked.
    assert_eq!(wish("avc1.640029").receivers, Some(vec![]));
    assert_eq!(wish("").receivers, None);
}

#[test]
fn the_sound_is_what_the_list_takes_or_none() {
    // Safari 14 to 16: the player drops `opus` and what follows it.
    let safari = wish("avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac");
    assert_eq!(safari.want.audio, one_of(&[Codec::Aac, Codec::Pcma, Codec::Pcmu, Codec::L16]));
    // Safari 13: no FLAC either.
    assert_eq!(wish("avc1.640029,mp4a.40.2,mp4a.40.5").want.audio, one_of(&[Codec::Aac]));
    // G.711 and L16 come only as FLAC.
    assert_eq!(wish("avc1.640029,flac").want.audio, one_of(&PCM));
    // No sound in the list: a picture alone.
    assert_eq!(wish("avc1.640029").want.audio, MediaWant::Skip);
    // AAC is `mp4a.40.*`; MP3 in MP4 (`mp4a.6B`, `mp4a.69`) is not sent.
    assert_eq!(wish("mp4a.6B,mp4a.69").want.audio, MediaWant::Skip);
    assert_eq!(wish("mp4a.40").want.audio, one_of(&[Codec::Aac]));
}

#[test]
fn a_list_of_sound_alone_wants_no_picture() {
    let w = wish("mp4a.40.2,opus");
    assert_eq!((w.want.video, w.want.audio), (MediaWant::Skip, one_of(&[Codec::Opus, Codec::Aac])));
}

#[test]
fn an_empty_list_asks_what_go2rtc_gives_and_an_unknown_one_nothing() {
    let w = wish("");
    assert_eq!(w.want, Want { video: one_of(&[Codec::H264, Codec::H265]), audio: one_of(&[Codec::Aac]) });
    assert_eq!(wish(" , ,").want, w.want, "names that are all blank are no list");
    assert!(wish("vp09.00.10.08,av01.0.04M.08").want.is_empty(), "nothing this server sends");
}

#[test]
fn a_long_list_is_read_up_to_its_bound() {
    let mut list = vec!["vp8"; MAX_CODECS];
    list.push("avc1.640029");
    assert!(wish(&list.join(",")).want.is_empty(), "names past the bound are not looked at");
    let repeated = vec!["avc1.640029"; 10_000].join(",");
    assert_eq!(wish(&repeated).want.video, one_of(&[Codec::H264]), "each codec once");
}
