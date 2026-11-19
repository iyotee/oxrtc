//! Session descriptions, with `sdp-types`: written for `DESCRIBE`, read from
//! `ANNOUNCE`.

use std::fmt::Write as _;
use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use oxrtc_core::{Codec, CodecParams, MediaKind, TrackInfo};
use sdp_types::{AddrType, Attribute, Connection, Media, NetType, Origin, Session, Time};

use crate::packetize::payload_type;
use oxrtc_depacketize::DepacketizerSpec;

/// Whether a track of this codec can be published into this server (`ANNOUNCE`):
/// the server sends Motion JPEG to its readers, and nothing publishes it.
pub(crate) fn publishable(codec: Codec) -> bool {
    !matches!(codec, Codec::Jpeg)
}

/// The control attribute of track `index`, and what `SETUP` URLs end with.
pub(crate) fn control(index: usize) -> String {
    format!("trackID={index}")
}

fn attribute(name: &str, value: impl Into<String>) -> Attribute {
    Attribute { attribute: name.to_owned(), value: Some(value.into()) }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02X}");
        s
    })
}

fn rtpmap(track: &TrackInfo) -> String {
    let rate = track.clock_rate;
    let ch = track.channels.max(1);
    match track.codec {
        Codec::H264 => format!("H264/{rate}"),
        Codec::H265 => format!("H265/{rate}"),
        Codec::Jpeg => format!("JPEG/{rate}"),
        Codec::Opus => "opus/48000/2".to_owned(),
        Codec::Pcma | Codec::Pcmu if ch == 1 => format!("{}/{rate}", track.codec.as_str().to_ascii_uppercase()),
        Codec::Pcma | Codec::Pcmu => format!("{}/{rate}/{ch}", track.codec.as_str().to_ascii_uppercase()),
        Codec::L16 => format!("L16/{rate}/{ch}"),
        Codec::Aac => format!("mpeg4-generic/{rate}/{ch}"),
    }
}

fn fmtp(track: &TrackInfo) -> Option<String> {
    match (track.codec, &track.params) {
        (Codec::H264, CodecParams::H264 { sps, pps }) => {
            let mut f =
                format!("packetization-mode=1;sprop-parameter-sets={},{}", STANDARD.encode(sps), STANDARD.encode(pps));
            if let Some(profile) = sps.get(1..4) {
                let _ = write!(f, ";profile-level-id={}", hex(profile));
            }
            Some(f)
        }
        (Codec::H264, _) => Some("packetization-mode=1".to_owned()),
        (Codec::H265, CodecParams::H265 { vps, sps, pps }) => Some(format!(
            "sprop-vps={};sprop-sps={};sprop-pps={}",
            STANDARD.encode(vps),
            STANDARD.encode(sps),
            STANDARD.encode(pps)
        )),
        (Codec::Aac, params) => {
            let mut f = "streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3"
                .to_owned();
            if let CodecParams::Aac { config } = params {
                let _ = write!(f, ";config={}", hex(config));
            }
            Some(f)
        }
        (Codec::Opus, _) if track.channels == 2 => Some("sprop-stereo=1".to_owned()),
        _ => None,
    }
}

/// The fastest Motion JPEG pictures dated as they arrive whose pace is
/// announced (`a=framerate`, RFC 8866): ten a second. A Motion JPEG picture
/// carries no frame rate, and a webcam's over HTTP is the pace its pictures
/// arrive at. FFmpeg, reading such a track, waits for 20 pictures to guess
/// one unless told (`-fpsprobesize`; 20 s for a webcam that sends one a
/// second). The relay's FFmpeg is told not to wait (`crates/ffmpeg`,
/// `RELAY_INPUT`), and this says the pace where its own guess found one: on
/// the owner's webcams it read 1 picture a second for the one that sends 1
/// and nothing for those that send 25 and 30 (its output then says `90k
/// tbr`, as it does in front of go2rtc), so a faster one is announced nothing
/// and its transcode is as it was (D36).
const ANNOUNCED_INTERVAL: Duration = Duration::from_millis(100);

/// The `a=framerate` value of a video track whose pictures are dated every
/// `interval`, on `grid` when their dates sit on one.
///
/// The grid comes first, for any codec: it is what the guess this line
/// replaces reads. FFmpeg's guess takes the greatest common step of the
/// dates, so a camera that stamps 25 slots a second and fills one in three
/// (the owner's ESP32: 8 pictures a second, dated 80, 120 or 160 ms apart)
/// reads `25/1`; announced at its pace instead, its transcode was encoded at
/// 8 pictures a second and twice the bits (W4 review, D36). It is also the
/// safe side for an encoder that outputs at a constant rate: a camera that
/// fills more slots later is not cut down to the rate it started at.
///
/// With no grid: a coded track (H.264, H.265) is announced at its pace,
/// whatever it is - few cameras write a rate in their parameter sets (the
/// owner's H.265 cameras do not: read without the guess and without this
/// line, their transcode was encoded as 90000 pictures a second, at half the
/// bits); a Motion JPEG track only from [`ANNOUNCED_INTERVAL`] up.
/// Nothing for a pace that would read as zero.
fn framerate(track: &TrackInfo, interval: Option<Duration>, grid: Option<Duration>) -> Option<String> {
    let interval = grid.or_else(|| interval.filter(|i| track.codec != Codec::Jpeg || *i >= ANNOUNCED_INTERVAL))?;
    let rate = format!("{:.3}", 1.0 / interval.as_secs_f64());
    rate.parse::<f64>().is_ok_and(|r| r.is_finite() && r > 0.0).then_some(rate)
}

/// The SDP describing `tracks` (already in order, video first), the first
/// video track's pictures coming every `picture_interval` when that is
/// known, dated on `grid` when they are.
pub(crate) fn describe(tracks: &[TrackInfo], picture_interval: Option<Duration>, grid: Option<Duration>) -> Vec<u8> {
    let first_video = tracks.iter().position(|t| t.kind == MediaKind::Video);
    let medias = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let pt = payload_type(index, track);
            let mut attributes = vec![attribute("rtpmap", format!("{pt} {}", rtpmap(track)))];
            if let Some(f) = fmtp(track) {
                attributes.push(attribute("fmtp", format!("{pt} {f}")));
            }
            if first_video == Some(index)
                && let Some(rate) = framerate(track, picture_interval, grid)
            {
                attributes.push(attribute("framerate", rate));
            }
            attributes.push(attribute("control", control(index)));
            Media {
                media: track.kind.as_str().to_owned(),
                port: 0,
                num_ports: None,
                proto: "RTP/AVP".to_owned(),
                fmt: pt.to_string(),
                media_title: None,
                connections: Vec::new(),
                bandwidths: Vec::new(),
                key: None,
                attributes,
            }
        })
        .collect();
    let session = Session {
        origin: Origin {
            username: Some("-".to_owned()),
            sess_id: fastrand::u32(..).to_string(),
            sess_version: 1,
            nettype: NetType::In,
            addrtype: AddrType::Ip4,
            unicast_address: "127.0.0.1".to_owned(),
        },
        session_name: "oxrtc".to_owned(),
        session_description: None,
        uri: None,
        emails: Vec::new(),
        phones: Vec::new(),
        connection: Some(Connection {
            nettype: NetType::In,
            addrtype: AddrType::Ip4,
            connection_address: "0.0.0.0".to_owned(),
        }),
        bandwidths: Vec::new(),
        times: vec![Time { start_time: 0, stop_time: 0, repeats: Vec::new() }],
        time_zones: Vec::new(),
        key: None,
        attributes: vec![attribute("tool", "oxrtc")],
        medias,
    };
    let mut out = Vec::with_capacity(512);
    // Writing into a Vec cannot fail for want of space.
    let _ = session.write(&mut out);
    out
}

/// One usable media of an announced SDP.
#[derive(Debug, Clone)]
pub(crate) struct Announced {
    /// Its `a=control`, if any.
    pub(crate) control: Option<String>,
    /// The track it becomes (without codec parameters: the depacketizer
    /// knows them).
    pub(crate) info: TrackInfo,
    /// How to depacketize it.
    pub(crate) spec: DepacketizerSpec,
}

/// Reads an announced SDP: the medias this server can carry, at most
/// `max_tracks` of them. Others are skipped.
pub(crate) fn parse_announce(body: &[u8], max_tracks: usize) -> Result<Vec<Announced>, String> {
    let session = Session::parse(body).map_err(|e| format!("bad SDP: {e}"))?;
    Ok(session.medias.iter().filter_map(announced).take(max_tracks).collect())
}

fn announced(media: &Media) -> Option<Announced> {
    let kind = match media.media.as_str() {
        "video" => MediaKind::Video,
        "audio" => MediaKind::Audio,
        _ => return None,
    };
    let pt = media.fmt.split_whitespace().next()?;
    let prefixed = |name: &str| {
        media.attributes.iter().filter(|a| a.attribute == name).find_map(|a| {
            let value = a.value.as_deref()?;
            let (key, rest) = value.split_once(' ')?;
            (key == pt).then(|| rest.trim().to_owned())
        })
    };
    let (encoding, clock, channels) = match prefixed("rtpmap") {
        Some(map) => {
            let mut parts = map.split('/');
            let encoding = parts.next()?.trim().to_ascii_lowercase();
            let clock: u32 = parts.next()?.trim().parse().ok()?;
            let channels: Option<u16> = parts.next().and_then(|c| c.trim().parse().ok());
            (encoding, clock, channels)
        }
        None => match pt {
            "0" => ("pcmu".to_owned(), 8000, Some(1)),
            "8" => ("pcma".to_owned(), 8000, Some(1)),
            _ => return None,
        },
    };
    let codec = Codec::parse(&encoding)?;
    if codec.kind() != kind || !publishable(codec) {
        return None;
    }
    let clock_rate = NonZeroU32::new(clock)?;
    let channels = channels.and_then(NonZeroU16::new);
    let info = match kind {
        MediaKind::Video => TrackInfo { clock_rate: clock, ..TrackInfo::video(codec) },
        MediaKind::Audio => TrackInfo::audio(codec, clock, channels.map_or(1, NonZeroU16::get)),
    };
    let control = media.attributes.iter().find(|a| a.attribute == "control").and_then(|a| a.value.clone());
    let spec = DepacketizerSpec { media: media.media.clone(), encoding, clock_rate, channels, fmtp: prefixed("fmtp") };
    Some(Announced { control, info, spec })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn text(tracks: &[TrackInfo]) -> String {
        String::from_utf8(describe(tracks, None, None)).unwrap()
    }

    #[test]
    fn h264_with_parameter_sets() {
        let sps = Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f, 0xac]);
        let pps = Bytes::from_static(&[0x68, 0xee, 0x3c, 0xb0]);
        let sdp = text(&[TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps, pps })]);
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"), "{sdp}");
        assert!(
            sdp.contains(
                "a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z2QAH6w=,aO48sA==;profile-level-id=64001F"
            ),
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
    fn a_slow_webcams_pace_is_announced_and_a_fast_ones_is_not() {
        let jpeg = [TrackInfo::video(Codec::Jpeg), TrackInfo::audio(Codec::Pcma, 8000, 1)];
        let one_a_second = String::from_utf8(describe(&jpeg, Some(Duration::from_secs(1)), None)).unwrap();
        let video = "m=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\na=framerate:1.000\r\na=control:trackID=0\r\n";
        assert!(one_a_second.contains(video), "{one_a_second}");
        assert_eq!(one_a_second.matches("a=framerate").count(), 1, "the sound has none: {one_a_second}");
        let eight = String::from_utf8(describe(&jpeg, Some(Duration::from_millis(120)), None)).unwrap();
        assert!(eight.contains("a=framerate:8.333\r\n"), "{eight}");
        for fast in [Some(Duration::from_millis(40)), Some(Duration::from_millis(33)), None] {
            let sdp = String::from_utf8(describe(&jpeg, fast, None)).unwrap();
            assert!(!sdp.contains("framerate"), "{fast:?}: {sdp}");
        }
    }

    #[test]
    fn a_coded_tracks_pace_is_announced_whatever_it_is() {
        // 70 ms: what the owner's H.265 cameras send, and do not say in their
        // parameter sets.
        let tracks = [TrackInfo::video(Codec::H265), TrackInfo::audio(Codec::Pcma, 8000, 1)];
        let sdp = String::from_utf8(describe(&tracks, Some(Duration::from_millis(70)), None)).unwrap();
        assert!(sdp.contains("a=framerate:14.286\r\na=control:trackID=0\r\n"), "{sdp}");
        assert_eq!(sdp.matches("a=framerate").count(), 1, "the sound has none: {sdp}");
        for (interval, said) in [(40, "25.000"), (1000, "1.000"), (33, "30.303")] {
            let h264 = [TrackInfo::video(Codec::H264)];
            let sdp = String::from_utf8(describe(&h264, Some(Duration::from_millis(interval)), None)).unwrap();
            assert!(sdp.contains(&format!("a=framerate:{said}\r\n")), "{interval} ms: {sdp}");
        }
        assert!(!text(&tracks).contains("framerate"), "no pace known, none said");
        // A pace that would read as none at all is not said.
        for never in [Duration::ZERO, Duration::from_secs(4000), Duration::MAX] {
            let jpeg = [TrackInfo::video(Codec::Jpeg)];
            for tracks in [&tracks[..1], &jpeg[..]] {
                let sdp = String::from_utf8(describe(tracks, Some(never), None)).unwrap();
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
        let sdp = String::from_utf8(describe(&jpeg, pace, slot)).unwrap();
        assert!(sdp.contains("a=framerate:25.000\r\n"), "{sdp}");
        // A Motion JPEG camera with a clock of its own that fills every slot
        // is faster than ten a second and announced all the same: its dates
        // say its rate, where a webcam's arrivals do not.
        let sdp = String::from_utf8(describe(&jpeg, slot, slot)).unwrap();
        assert!(sdp.contains("a=framerate:25.000\r\n"), "{sdp}");
        // A coded camera that sends every other slot of its grid.
        let h265 = [TrackInfo::video(Codec::H265)];
        let (pace, slot) = (Some(Duration::from_millis(140)), Some(Duration::from_millis(70)));
        let sdp = String::from_utf8(describe(&h265, pace, slot)).unwrap();
        assert!(sdp.contains("a=framerate:14.286\r\n"), "{sdp}");
    }

    #[test]
    fn a_published_jpeg_is_refused() {
        // The server sends Motion JPEG and has no depacketizer for it.
        let sdp = describe(&[TrackInfo::video(Codec::Jpeg), TrackInfo::audio(Codec::Pcma, 8000, 1)], None, None);
        let announced = parse_announce(&sdp, 8).unwrap();
        assert_eq!(announced.len(), 1);
        assert_eq!(announced[0].info.codec, Codec::Pcma);
    }

    #[test]
    fn our_own_description_reads_back() {
        let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
        let announced = parse_announce(&describe(&tracks, None, None), 8).unwrap();
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
}
