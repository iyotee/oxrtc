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

/// The `a=framerate` value of a video track whose pictures are dated every
/// `interval` (the median gap), come at `rate` (the mean gap that is no
/// pause, a whole number of pictures a second when near one), and sit on
/// `grid` when their dates do.
///
/// A Motion JPEG picture carries no frame rate, and a webcam's over HTTP is
/// dated as it arrives. FFmpeg, reading such a track, waits for 20 pictures
/// to guess one unless told (`-fpsprobesize`; 20 s for a webcam that sends
/// one a second); the relay's FFmpeg is told not to wait (`crates/ffmpeg`,
/// `RELAY_INPUT`), so the rate x264 encodes at is the one said here, or 90000
/// pictures a second when nothing is said, which it turns into pictures of a
/// third of the bits.
///
/// The grid comes first, for any codec: it is what FFmpeg's own guess reads,
/// and where the pictures come evenly it is the pace (the owner's H.265
/// cameras; his webcams on an even minute: the guess read `30/1` and `25/1`
/// of them). The guess takes the step the dates share, so a camera that
/// stamps 25 slots a second and fills one in three (the owner's ESP32: 8
/// pictures a second, dated 80, 120 or 160 ms apart) reads `25/1`; announced
/// at its pace instead, its transcode was encoded at 8 pictures a second and
/// twice the bits (W4 review, D36). It is also the safe side for an encoder
/// that outputs at a constant rate: a camera that fills more slots later is
/// not cut down to the rate it started at.
///
/// With no grid: a coded track (H.264, H.265) is announced at its pace,
/// whatever it is - few cameras write a rate in their parameter sets (the
/// owner's H.265 cameras do not: read without the guess and without this
/// line, their transcode was encoded as 90000 pictures a second, at half the
/// bits). A Motion JPEG track is announced at its rate, however fast (the
/// owner, 2026-11-20: "toujours la cadence", always give x264 the camera's
/// rate). Until then a webcam faster than ten pictures a second was announced
/// only where FFmpeg's guess found a rate, as go2rtc's transcodes have it: a
/// 30-a-second webcam was encoded at 7 Mbit/s on a minute when its pictures
/// came evenly and at 2.9 on the others, by the half second its transcode
/// started in (D36). Nothing for a pace that would read as zero.
fn framerate(
    track: &TrackInfo,
    interval: Option<Duration>,
    rate: Option<Duration>,
    grid: Option<Duration>,
) -> Option<String> {
    let unguessed = if track.codec == Codec::Jpeg { rate.or(interval) } else { interval };
    let interval = grid.or(unguessed)?;
    let rate = format!("{:.3}", 1.0 / interval.as_secs_f64());
    rate.parse::<f64>().is_ok_and(|r| r.is_finite() && r > 0.0).then_some(rate)
}

/// The SDP describing `tracks` (already in order, video first), the first
/// video track's pictures coming every `picture_interval` (their median gap)
/// at `rate` (their mean gap that is no pause) when those are known, dated on
/// `grid` when they are ([`framerate`]).
pub(crate) fn describe(
    tracks: &[TrackInfo],
    picture_interval: Option<Duration>,
    rate: Option<Duration>,
    grid: Option<Duration>,
) -> Vec<u8> {
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
                && let Some(said) = framerate(track, picture_interval, rate, grid)
            {
                attributes.push(attribute("framerate", said));
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
#[path = "sdp_tests.rs"]
mod tests;
