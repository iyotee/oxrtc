//! A reading session: `DESCRIBE` subscribes, `SETUP` chooses the tracks,
//! `PLAY` starts sending.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::BytesMut;
use oxrtc_core::{
    ConsumerClass, ConsumerTag, Event, Frame, MediaKind, SubEnd, SubscribeError, Subscription, TrackInfo, Want,
};
use rtsp_types::StatusCode;
use rtsp_types::headers::{CONTENT_BASE, CONTENT_TYPE, RANGE, RTP_INFO, SESSION, TRANSPORT, USER_AGENT};
use tokio::time::Instant;

use crate::Shared;
use crate::conn::{Reply, Req, interleaved, target};
use crate::outbox::Outbox;
use crate::packetize::{Packetizer, payload_type};
use crate::publish::PUBLISH_PREFIX;
use crate::sdp::describe;
use crate::wire::reply;

/// Longest wait for a stream's tracks on `DESCRIBE`.
pub(crate) const DESCRIBE_WAIT: Duration = Duration::from_secs(29);
/// Interval between RTCP sender reports.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// The sending side of a reader: packetizers and the drop policy.
#[derive(Debug)]
pub(crate) struct Egress {
    tracks: Arc<[TrackInfo]>,
    packetizers: Vec<Option<Packetizer>>,
    waiting_key: bool,
    dropped: u64,
    next_report: Instant,
}

impl Egress {
    pub(crate) fn new(tracks: Arc<[TrackInfo]>) -> Self {
        let packetizers = tracks.iter().map(|_| None).collect();
        Self { tracks, packetizers, waiting_key: false, dropped: 0, next_report: Instant::now() + REPORT_INTERVAL }
    }

    /// Frames dropped because the reader was too slow.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Packetizes `frame` of `track` and offers it to the queue. When the queue
    /// is full the frame is dropped and video waits for the next keyframe.
    pub(crate) fn send(&mut self, track: usize, frame: &Frame, outbox: &Outbox) {
        let Some(info) = self.tracks.get(track) else { return };
        if self.packetizers.get(track).is_none_or(Option::is_none) {
            return;
        }
        let video = info.kind == MediaKind::Video;
        if video && self.waiting_key && !frame.keyframe {
            self.dropped += 1;
            return;
        }
        let mut out = BytesMut::with_capacity(frame.data.len() + 128);
        let now = Instant::now();
        let report_due = now >= self.next_report;
        if report_due {
            self.next_report = now + REPORT_INTERVAL;
            let wall = SystemTime::now();
            for packetizer in self.packetizers.iter().flatten() {
                packetizer.sender_report(wall, &mut out);
            }
        }
        let mut first = false;
        if let Some(Some(packetizer)) = self.packetizers.get_mut(track) {
            first = !packetizer.started();
            packetizer.packetize(frame, &info.params, &mut out);
        }
        if out.is_empty() {
            return;
        }
        if outbox.try_media(out.freeze()) {
            if video && frame.keyframe {
                self.waiting_key = false;
            }
        } else {
            self.dropped += 1;
            self.waiting_key = true;
            // The reports went with the frame: the track's first one is what
            // dates its packets for the reader (FFmpeg dates a track without
            // one by its own first packet, then moves it when one comes), so
            // it goes again with the next frame, as a due periodic one does.
            if first && let Some(Some(packetizer)) = self.packetizers.get_mut(track) {
                packetizer.unsent_first();
            }
            if report_due {
                self.next_report = now;
            }
        }
    }
}

/// A reading session.
pub(crate) struct Reader {
    sub: Subscription,
    base: String,
    egress: Egress,
    playing: bool,
    deadline: Option<Instant>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.egress.dropped() > 0 {
            tracing::debug!(stream = self.sub.stream().name(), dropped = self.egress.dropped(), "rtsp: slow reader");
        }
    }
}

fn status_of(error: &SubscribeError) -> StatusCode {
    match error {
        SubscribeError::NothingWanted => StatusCode::BadRequest,
        SubscribeError::NoMatchingSource | SubscribeError::Closed => StatusCode::NotFound,
        SubscribeError::TooManyConsumers => StatusCode::ServiceUnavailable,
    }
}

/// The track a `SETUP` URL names: the number after the last `trackID=`.
fn track_of(uri: &str) -> Option<usize> {
    let (_, rest) = uri.rsplit_once("trackID=")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).take(3).collect();
    digits.parse().ok()
}

impl Reader {
    /// Subscribes to the stream the URL names and describes its tracks.
    pub(crate) async fn describe(shared: &Shared, peer: SocketAddr, request: &Req) -> Result<(Self, Reply), Reply> {
        let fail = |status| reply(request, status).build(Vec::new());
        let (name, query) = target(request).ok_or_else(|| fail(StatusCode::BadRequest))?;
        if name.starts_with(PUBLISH_PREFIX) {
            return Err(fail(StatusCode::Forbidden));
        }
        let stream = shared.hub.get(&name).ok_or_else(|| fail(StatusCode::NotFound))?;
        let want = Want::from_query(&query).map_err(|_| fail(StatusCode::BadRequest))?;
        // A reader writes what it reads (the NVR's segmenter, a sound
        // transcode): the deep queue by default, so a pause of a few seconds
        // costs no frame. One that shows what it reads (a detector, a video
        // transcode: the `ffmpeg:` driver says it for one) says `queue=live`,
        // and is cut to a keyframe rather than left seconds behind.
        let class = ConsumerClass::from_query(&query).map_err(|_| fail(StatusCode::BadRequest))?;
        let mut tag =
            ConsumerTag::new("rtsp").with_remote(peer.to_string()).with_class(class.unwrap_or(ConsumerClass::Recorder));
        if let Some(agent) = request.header(&USER_AGENT) {
            tag = tag.with_user_agent(agent.as_str());
        }
        let wait = async {
            let mut sub = stream.subscribe(&want, tag).await.map_err(|e| status_of(&e))?;
            loop {
                match sub.recv().await {
                    Ok(Event::Tracks(tracks)) => return Ok((sub, tracks)),
                    Ok(_) => {}
                    Err(SubEnd::Closed) => return Err(StatusCode::NotFound),
                    Err(SubEnd::NoTracks) => return Err(StatusCode::ServiceUnavailable),
                }
            }
        };
        let (sub, tracks) = match tokio::time::timeout(DESCRIBE_WAIT, wait).await {
            Ok(Ok(found)) => found,
            Ok(Err(status)) => return Err(fail(status)),
            Err(_) => return Err(fail(StatusCode::ServiceUnavailable)),
        };
        if tracks.is_empty() {
            return Err(fail(StatusCode::UnsupportedMediaType));
        }
        let uri = request.request_uri().map(|u| u.as_str().to_owned()).unwrap_or_default();
        let base = if uri.ends_with('/') { uri } else { format!("{uri}/") };
        let sdp = describe(&tracks, sub.picture_interval());
        let response = reply(request, StatusCode::Ok)
            .header(CONTENT_TYPE, "application/sdp")
            .header(CONTENT_BASE, base.as_str())
            .build(sdp);
        let deadline = Instant::now().checked_add(shared.config.setup_play_timeout);
        Ok((Self { sub, base, egress: Egress::new(tracks), playing: false, deadline }, response))
    }

    /// When the session is dropped for not reaching `PLAY`.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn playing(&self) -> bool {
        self.playing
    }

    pub(crate) async fn recv(&mut self) -> Result<Event, SubEnd> {
        self.sub.recv().await
    }

    pub(crate) fn setup(&mut self, request: &Req, session: &str) -> Reply {
        let fail = |status| reply(request, status).build(Vec::new());
        let Some(transport) = interleaved(request) else { return fail(StatusCode::UnsupportedTransport) };
        if transport.record {
            return fail(StatusCode::UnsupportedTransport);
        }
        let uri = request.request_uri().map(|u| u.as_str().to_owned()).unwrap_or_default();
        let index = match track_of(&uri) {
            Some(index) => index,
            // No control in the URL: the first track not yet set up.
            None => match self.egress.packetizers.iter().position(Option::is_none) {
                Some(index) => index,
                None => return fail(StatusCode::NotFound),
            },
        };
        let Some(track) = self.egress.tracks.get(index) else {
            return fail(StatusCode::NotFound);
        };
        let channels = transport.channels.unwrap_or_else(|| {
            let rtp = u8::try_from(index * 2).unwrap_or(0);
            (rtp, rtp.wrapping_add(1))
        });
        // Two tracks on one channel could not be told apart by the client.
        let taken = self.egress.packetizers.iter().enumerate().any(|(i, p)| {
            i != index && p.as_ref().is_some_and(|p| [p.channel(), p.rtcp_channel()].contains(&channels.0))
        });
        if taken {
            return fail(StatusCode::BadRequest);
        }
        let packetizer = Packetizer::new(track, payload_type(index, track), channels.0).with_rtcp_channel(channels.1);
        let header = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={:08X};mode=play",
            channels.0,
            channels.1,
            packetizer.ssrc()
        );
        if let Some(slot) = self.egress.packetizers.get_mut(index) {
            *slot = Some(packetizer);
        }
        reply(request, StatusCode::Ok).header(TRANSPORT, header).header(SESSION, session).build(Vec::new())
    }

    pub(crate) fn play(&mut self, request: &Req, session: &str) -> Reply {
        if self.egress.packetizers.iter().all(Option::is_none) {
            return reply(request, StatusCode::MethodNotValidInThisState).build(Vec::new());
        }
        let rtp_info: Vec<String> = self
            .egress
            .packetizers
            .iter()
            .enumerate()
            .filter_map(|(index, p)| {
                p.as_ref().map(|p| format!("url={}{};seq={}", self.base, crate::sdp::control(index), p.initial_seq()))
            })
            .collect();
        self.playing = true;
        self.deadline = None;
        reply(request, StatusCode::Ok)
            .header(SESSION, session)
            .header(RANGE, "npt=0.000-")
            .header(RTP_INFO, rtp_info.join(","))
            .build(Vec::new())
    }

    /// Handles one subscription event. An error ends the connection.
    pub(crate) fn on_event(&mut self, event: Result<Event, SubEnd>, outbox: &Outbox) -> Result<(), &'static str> {
        match event {
            Ok(Event::Frame { track, frame }) => self.egress.send(track, &frame, outbox),
            Ok(Event::Tracks(tracks)) => {
                // Same media, codecs, clock rates and channels: the
                // description still holds (new parameter sets are inserted in
                // band). Otherwise the reader must describe again: a packetizer
                // keeps the clock rate it was set up with, and the timeline
                // continues a track in its new one.
                let shape =
                    |t: &[TrackInfo]| t.iter().map(|t| (t.kind, t.codec, t.clock_rate, t.channels)).collect::<Vec<_>>();
                if shape(&tracks) != shape(&self.egress.tracks) {
                    return Err("stream tracks changed");
                }
                self.egress.tracks = tracks;
            }
            Ok(Event::Discontinuity) => {}
            Err(SubEnd::Closed) => return Err("stream closed"),
            Err(SubEnd::NoTracks) => return Err("no tracks"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use oxrtc_core::{Codec, CodecParams};

    use super::*;
    use crate::outbox::outbox;

    #[test]
    fn track_numbers_from_setup_urls() {
        assert_eq!(track_of("rtsp://h/cam?video/trackID=0"), Some(0));
        assert_eq!(track_of("rtsp://h/cam/trackID=12"), Some(12));
        assert_eq!(track_of("rtsp://h/trackID=1/x"), Some(1));
        assert_eq!(track_of("rtsp://h/cam"), None);
        assert_eq!(track_of("rtsp://h/cam/trackID=99999999999"), Some(999));
    }

    fn egress() -> Egress {
        let tracks: Arc<[TrackInfo]> =
            vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Pcma, 8000, 1)].into();
        let mut egress = Egress::new(tracks.clone());
        egress.packetizers =
            tracks.iter().enumerate().map(|(i, t)| Some(Packetizer::new(t, 96, 2 * i as u8))).collect();
        egress
    }

    fn video(key: bool) -> Frame {
        Frame::new(0, 0, key, Bytes::from_static(&[0, 0, 0, 3, 0x65, 1, 2]))
    }

    #[test]
    fn a_full_queue_drops_until_the_next_keyframe() {
        let (out, _inbox) = outbox(3, usize::MAX);
        let mut e = egress();
        for _ in 0..3 {
            e.send(0, &video(false), &out);
        }
        assert_eq!(e.dropped(), 0);
        // Queue full: dropped, and now waiting for a keyframe.
        e.send(0, &video(false), &out);
        assert_eq!(e.dropped(), 1);
        drop(_inbox);
        let (out, _inbox) = outbox(16, usize::MAX);
        e.send(0, &video(false), &out);
        assert_eq!(e.dropped(), 2, "delta frames dropped while waiting");
        e.send(1, &Frame::new(1, 0, true, Bytes::from_static(&[0xd5; 160])), &out);
        assert_eq!(e.dropped(), 2, "audio keeps flowing");
        e.send(0, &video(true), &out);
        e.send(0, &video(false), &out);
        assert_eq!(e.dropped(), 2, "resumed at the keyframe");
    }

    /// Whether an interleaved item is a sender report on `channel`.
    fn is_report(item: &[u8], channel: u8) -> bool {
        item.get(1) == Some(&channel) && item.get(5) == Some(&200)
    }

    #[test]
    fn a_tracks_first_report_is_sent_again_when_its_frame_was_dropped() {
        // The queue is full of the cached GOP when the sound's first frame
        // comes: that frame and the report before it are dropped.
        let (out, _full) = outbox(1, usize::MAX);
        let mut e = egress();
        let sound = Frame::new(1, 0, true, Bytes::from_static(&[0xd5; 160]));
        e.send(0, &video(true), &out);
        e.send(1, &sound, &out);
        assert_eq!(e.dropped(), 1);
        // Room again: the next sound frame is the first the reader gets, and
        // it must come after its report, not 5 s later.
        let (out, mut inbox) = outbox(16, usize::MAX);
        e.send(1, &sound, &out);
        let item = inbox.try_recv_for_test().unwrap();
        assert!(is_report(&item, 3), "the sound's first packet went out without a sender report");
    }

    #[test]
    fn periodic_reports_dropped_with_a_frame_are_sent_with_the_next_one() {
        let (out, mut inbox) = outbox(16, usize::MAX);
        let mut e = egress();
        e.send(0, &video(true), &out);
        while inbox.try_recv_for_test().is_some() {}
        // A report is due, and the frame that would carry it is dropped.
        e.next_report = Instant::now();
        let (full, _full) = outbox(1, usize::MAX);
        assert!(full.try_media(Bytes::from_static(b"queued")));
        e.send(0, &video(false), &full);
        assert_eq!(e.dropped(), 1);
        e.send(0, &video(true), &out);
        let item = inbox.try_recv_for_test().unwrap();
        assert!(is_report(&item, 1), "the report waited another interval");
    }

    #[test]
    fn tracks_not_set_up_are_not_sent() {
        let (out, mut inbox) = outbox(16, usize::MAX);
        let tracks: Arc<[TrackInfo]> = vec![TrackInfo::video(Codec::H264).with_params(CodecParams::None)].into();
        let mut e = Egress::new(tracks);
        e.send(0, &video(true), &out);
        e.send(5, &video(true), &out);
        drop(out);
        assert!(inbox_is_empty(&mut inbox));
    }

    fn inbox_is_empty(inbox: &mut crate::outbox::Inbox) -> bool {
        inbox.try_recv_for_test().is_none()
    }
}
