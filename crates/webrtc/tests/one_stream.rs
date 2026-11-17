//! One MediaStream per session: every media line of an answer names the same
//! stream (`a=msid`, the `a=ssrc ... msid:` attributes of the picture, its
//! retransmissions and the sound, and `a=msid-semantic`), with a track of its
//! own. A browser builds one `MediaStream` per stream id: with one id per
//! line, a player that shows `event.streams[0]` of every track event ends on
//! the sound's stream and shows no picture (openIVR's live view, 2026-11-17).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use oxrtc_core::{Codec, Stream};
use oxrtc_webrtc::WebRtcServer;
use str0m::RtcConfig;
use str0m::media::{Direction, MediaKind as RtcKind};

const WAIT: Duration = Duration::from_secs(10);

/// What one media line of an answer says of its stream and track.
#[derive(Debug, Default)]
struct Line {
    kind: String,
    rejected: bool,
    /// Every `a=msid:<stream> <track>` of the line.
    msids: Vec<(String, String)>,
    /// Every `a=ssrc:<ssrc> msid:<stream> <track>` of the line.
    ssrc_msids: Vec<(String, String, String)>,
    /// The SSRCs of `a=ssrc-group:FID`: the picture's and its retransmissions'.
    fid: Vec<String>,
}

#[derive(Debug, Default)]
struct Answer {
    /// The stream ids of `a=msid-semantic: WMS ...`.
    semantic: Vec<String>,
    lines: Vec<Line>,
}

fn read(answer: &str) -> Answer {
    let mut out = Answer::default();
    for line in answer.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            let mut fields = media.split(' ');
            let kind = fields.next().unwrap().to_owned();
            let rejected = fields.next() == Some("0");
            out.lines.push(Line { kind, rejected, ..Line::default() });
        } else if let Some(rest) = line.strip_prefix("a=msid-semantic:") {
            let mut ids = rest.split_whitespace();
            assert_eq!(ids.next(), Some("WMS"), "{line}");
            out.semantic = ids.map(str::to_owned).collect();
        } else if let Some(rest) = line.strip_prefix("a=msid:") {
            let (stream, track) = rest.split_once(' ').unwrap_or_else(|| panic!("an msid without a track: {line}"));
            out.lines.last_mut().expect("a=msid inside a media line").msids.push((stream.into(), track.into()));
        } else if let Some(rest) = line.strip_prefix("a=ssrc-group:FID ") {
            out.lines.last_mut().unwrap().fid = rest.split(' ').map(str::to_owned).collect();
        } else if let Some(rest) = line.strip_prefix("a=ssrc:") {
            let (ssrc, attribute) = rest.split_once(' ').unwrap();
            if let Some(msid) = attribute.strip_prefix("msid:") {
                let (stream, track) = msid.split_once(' ').unwrap_or_else(|| panic!("no track: {line}"));
                out.lines.last_mut().unwrap().ssrc_msids.push((ssrc.into(), stream.into(), track.into()));
            }
        }
    }
    out
}

/// RFC 8830: `msid-id = 1*64token-char`.
fn is_msid_id(id: &str) -> bool {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`{|}~".contains(c);
    !id.is_empty() && id.len() <= 64 && id.chars().all(token)
}

/// Asserts the answer names ONE stream, on every media line it keeps, in
/// every attribute that names one, and returns that stream's id with the
/// track id of each kept line.
fn one_stream(answer: &str) -> (String, Vec<String>) {
    let read = read(answer);
    let kept: Vec<&Line> = read.lines.iter().filter(|l| !l.rejected).collect();
    assert!(!kept.is_empty(), "no media line kept:\n{answer}");
    let mut tracks = Vec::new();
    for line in &kept {
        assert_eq!(line.msids.len(), 1, "one a=msid per {} line:\n{answer}", line.kind);
        let (stream, track) = &line.msids[0];
        assert!(is_msid_id(stream) && is_msid_id(track), "{stream} {track}: not msid ids (RFC 8830)");
        assert!(!line.ssrc_msids.is_empty(), "the {} line names its SSRC's stream:\n{answer}", line.kind);
        for (ssrc, s, t) in &line.ssrc_msids {
            assert_eq!((s, t), (stream, track), "a=ssrc:{ssrc} msid disagrees with the line's a=msid:\n{answer}");
        }
        // The picture's retransmissions (RTX) are a second SSRC of the same
        // track: a browser that finds another stream there builds another.
        for ssrc in &line.fid {
            assert!(line.ssrc_msids.iter().any(|(s, ..)| s == ssrc), "SSRC {ssrc} of the FID group has no msid");
        }
        tracks.push(track.clone());
    }
    let streams: HashSet<&String> = kept.iter().map(|l| &l.msids[0].0).collect();
    assert_eq!(streams.len(), 1, "{} streams for {} media lines:\n{answer}", streams.len(), kept.len());
    let stream = kept[0].msids[0].0.clone();
    assert_eq!(read.semantic, vec![stream.clone()], "a=msid-semantic lists the session's stream alone:\n{answer}");
    let distinct: HashSet<&String> = tracks.iter().collect();
    assert_eq!(distinct.len(), tracks.len(), "each line has a track of its own: {tracks:?}");
    for line in read.lines.iter().filter(|l| l.rejected) {
        assert!(line.msids.is_empty() && line.ssrc_msids.is_empty(), "a refused line names no stream:\n{answer}");
    }
    (stream, tracks)
}

/// A media line of an offer: its kind, its direction, and the stream and
/// track ids the client gives it (a browser names them on a line it sends on).
type Offered = (RtcKind, Direction, Option<(&'static str, &'static str)>);

/// An offer from a str0m client. `browser`: as a browser writes a line it
/// only receives on, with no `a=msid` and no `a=ssrc` (str0m writes both on
/// every line of its own offers).
fn offer(lines: &[Offered], browser: bool) -> String {
    let config = RtcConfig::new().clear_codecs().enable_h264(true).enable_opus(true, false);
    let mut rtc = config.build(Instant::now());
    let mut api = rtc.sdp_api();
    for (kind, direction, ids) in lines {
        api.add_media(*kind, *direction, ids.map(|i| i.0.to_owned()), ids.map(|i| i.1.to_owned()), None);
    }
    let (offer, _pending) = api.apply().unwrap();
    let sdp = offer.to_sdp_string();
    if !browser {
        return sdp;
    }
    let kept: Vec<&str> =
        sdp.split_inclusive('\n').filter(|l| !l.starts_with("a=msid:") && !l.starts_with("a=ssrc")).collect();
    kept.concat()
}

const RECV: Direction = Direction::RecvOnly;

async fn camera() -> (WebRtcServer, Arc<Stream>) {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    (server(8, Duration::from_secs(10), WAIT).await, stream)
}

async fn answer(server: &WebRtcServer, stream: &Arc<Stream>, offer: &str) -> String {
    let caller: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    server.answer(stream.clone(), offer, Some(caller)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_picture_and_the_sound_are_one_stream() {
    let (server, stream) = camera().await;
    let sdp =
        answer(&server, &stream, &offer(&[(RtcKind::Video, RECV, None), (RtcKind::Audio, RECV, None)], true)).await;
    let (_, tracks) = one_stream(&sdp);
    assert_eq!(tracks.len(), 2, "the picture and the sound:\n{sdp}");
    let read = read(&sdp);
    assert_eq!(read.lines[0].fid.len(), 2, "the picture is answered with its retransmissions:\n{sdp}");
    assert_eq!(read.lines[0].ssrc_msids.len(), 2, "the picture's SSRC and its RTX SSRC both name the stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picture_alone_is_one_stream() {
    let (server, stream) = camera().await;
    let sdp = answer(&server, &stream, &offer(&[(RtcKind::Video, RECV, None)], true)).await;
    let (_, tracks) = one_stream(&sdp);
    assert_eq!(tracks.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_sound_lines_share_the_stream() {
    let (server, stream) = camera().await;
    let lines = [
        (RtcKind::Video, RECV, None),
        (RtcKind::Audio, RECV, None),
        (RtcKind::Audio, RECV, None),
        (RtcKind::Audio, RECV, None),
    ];
    let sdp = answer(&server, &stream, &offer(&lines, true)).await;
    let (_, tracks) = one_stream(&sdp);
    assert_eq!(tracks.len(), 4, "every offered line is answered:\n{sdp}");
}

/// str0m copies the `a=msid` of an offer's line into its answer: a browser
/// that sends on a line (a microphone) would see its own stream and track
/// ids come back as the server's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ids_are_the_servers_never_the_offers() {
    let (server, stream) = camera().await;
    let lines = [
        (RtcKind::Audio, Direction::SendRecv, Some(("offeredstream", "offeredsound"))),
        (RtcKind::Video, RECV, Some(("otheroffered", "offeredpicture"))),
    ];
    let sdp = answer(&server, &stream, &offer(&lines, false)).await;
    let (stream_id, tracks) = one_stream(&sdp);
    assert_eq!(tracks.len(), 2);
    for offered in ["offeredstream", "otheroffered", "offeredsound", "offeredpicture"] {
        assert!(!sdp.contains(offered), "the answer names the offer's own {offered}:\n{sdp}");
    }
    assert!(stream_id.len() >= 16, "an id of the server's own making: {stream_id}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_line_names_no_stream() {
    let (server, stream) = camera().await;
    // A line the browser only sends on: this server takes nothing from a
    // viewer, and refuses the line.
    let lines = [(RtcKind::Video, RECV, None), (RtcKind::Audio, Direction::SendOnly, Some(("mic", "mictrack")))];
    let sdp = answer(&server, &stream, &offer(&lines, false)).await;
    let (_, tracks) = one_stream(&sdp);
    assert_eq!(tracks.len(), 1, "the picture alone:\n{sdp}");
    assert!(read(&sdp).lines[1].rejected, "the microphone's line is refused:\n{sdp}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_session_has_a_stream_of_its_own() {
    let (server, stream) = camera().await;
    let lines = [(RtcKind::Video, RECV, None), (RtcKind::Audio, RECV, None)];
    let (first, _) = one_stream(&answer(&server, &stream, &offer(&lines, true)).await);
    let (second, _) = one_stream(&answer(&server, &stream, &offer(&lines, true)).await);
    assert_ne!(first, second, "two sessions, two streams: a page showing two cameras tells them apart");
}

/// The answer is still one a client takes and plays: the ids are rewritten
/// in the text str0m serialised, and its own client must read it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_plays_the_answer() {
    let (server, stream) = camera().await;
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    assert_video(&mut client, 5).await;
}
