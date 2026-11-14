//! The `StreamInfo` JSON of `GET /api/streams`, in go2rtc's shape.

use oxrtc_core::{Codec, MediaKind, MediaWant, SourceState, StreamSnapshot, TrackInfo, Want};
use serde::Serialize;

/// One stream, as openIVR reads it.
#[derive(Debug, Serialize)]
pub(crate) struct StreamInfo {
    producers: Vec<Producer>,
    consumers: Vec<Consumer>,
}

#[derive(Debug, Serialize)]
struct Producer {
    url: String,
    medias: Vec<String>,
    receivers: Vec<Receiver>,
}

#[derive(Debug, Serialize)]
struct Receiver {
    codec: CodecInfo,
    #[serde(skip_serializing_if = "is_zero")]
    packets: u64,
}

#[derive(Debug, Serialize)]
struct CodecInfo {
    codec_name: &'static str,
    codec_type: &'static str,
    clock_rate: u32,
}

#[derive(Debug, Serialize)]
struct Consumer {
    format_name: String,
    medias: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_addr: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// go2rtc's (SDP's) upper-case codec name.
fn sdp_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H264",
        Codec::H265 => "H265",
        Codec::Jpeg => "JPEG",
        Codec::Opus => "OPUS",
        Codec::Pcma => "PCMA",
        Codec::Pcmu => "PCMU",
        Codec::Aac => "MPEG4-GENERIC",
        Codec::L16 => "L16",
    }
}

/// `video, recvonly, H265` / `audio, recvonly, PCMA/16000` / `audio, recvonly, OPUS/48000/2`.
fn producer_media(track: &TrackInfo) -> String {
    let mut line = format!("{}, recvonly, {}", track.kind.as_str(), sdp_name(track.codec));
    if track.clock_rate != 0 && track.clock_rate != 90_000 {
        line.push_str(&format!("/{}", track.clock_rate));
    }
    if track.channels >= 2 {
        line.push_str(&format!("/{}", track.channels));
    }
    line
}

fn consumer_media(kind: MediaKind, codec: Option<Codec>) -> String {
    match codec {
        Some(codec) => format!("{}, sendonly, {}", kind.as_str(), sdp_name(codec)),
        None => format!("{}, sendonly", kind.as_str()),
    }
}

/// What a preload consumer lists: per wanted kind, the codec it got once its
/// tracks are known (a kind it did not get is left out), else the codec it
/// asked for.
pub(crate) fn preload_medias(want: &Want, tracks: Option<&[TrackInfo]>) -> Vec<String> {
    MediaKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let wish = want.get(kind);
            if !wish.is_wanted() {
                return None;
            }
            let got = tracks.and_then(|t| t.iter().find(|t| t.kind == kind && wish.accepts(t.codec)));
            let codec = match (got, wish) {
                (Some(track), _) => Some(track.codec),
                // Tracks known and none of this kind: not held, not listed.
                (None, _) if tracks.is_some() => return None,
                (None, MediaWant::OneOf(codecs)) => codecs.first().copied(),
                (None, _) => None,
            };
            Some(consumer_media(kind, codec))
        })
        .collect()
}

/// The JSON of one stream. `preload` is what the stream's preload lists, if
/// it has one.
pub(crate) fn stream_info(snapshot: &StreamSnapshot, preload: Option<&[String]>) -> StreamInfo {
    let producers = snapshot
        .sources
        .iter()
        .filter(|s| s.state == SourceState::Running || !s.tracks.is_empty())
        .map(|s| Producer {
            url: s.url.clone(),
            medias: s.tracks.iter().map(producer_media).collect(),
            receivers: s
                .tracks
                .iter()
                .enumerate()
                .map(|(i, t)| Receiver {
                    codec: CodecInfo {
                        codec_name: t.codec.as_str(),
                        codec_type: t.kind.as_str(),
                        clock_rate: t.clock_rate,
                    },
                    packets: s.track_frames.get(i).copied().unwrap_or(0),
                })
                .collect(),
        })
        .collect();
    let consumers = snapshot
        .consumers
        .iter()
        .map(|c| Consumer {
            format_name: c.kind.clone(),
            medias: match (c.kind.as_str(), preload) {
                ("preload", Some(medias)) => medias.to_vec(),
                _ => Vec::new(),
            },
            remote_addr: c.remote.clone(),
        })
        .collect();
    StreamInfo { producers, consumers }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_lines() {
        assert_eq!(producer_media(&TrackInfo::video(Codec::H265)), "video, recvonly, H265");
        assert_eq!(producer_media(&TrackInfo::audio(Codec::Pcma, 16_000, 1)), "audio, recvonly, PCMA/16000");
        assert_eq!(producer_media(&TrackInfo::audio(Codec::Opus, 48_000, 2)), "audio, recvonly, OPUS/48000/2");
        let want = Want::from_query("audio=opus").unwrap();
        assert_eq!(preload_medias(&want, None), vec!["audio, sendonly, OPUS"]);
        let want = Want::from_query("video&audio=opus").unwrap();
        let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
        assert_eq!(preload_medias(&want, Some(&tracks)), vec!["video, sendonly, H264", "audio, sendonly, OPUS"]);
        assert_eq!(preload_medias(&Want::from_query("video").unwrap(), None), vec!["video, sendonly"]);
    }
}
