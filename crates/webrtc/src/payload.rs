//! From subscription frames to what str0m's writer takes: the payload type
//! of each track, Annex B video with its parameter sets, and RTP time. The
//! frames that come while the session connects wait in the subscription's
//! own queue, under the live budget (`oxrtc_core::queue`): the session reads
//! it only once connected.
//!
//! The RTP time is the frame's `pts` as it is: the hub's timeline
//! ([`oxrtc_core::timeline`]) already never goes back and does not jump when
//! a source restarts, for this egress and the RTSP one alike.

use std::time::Instant;

use oxrtc_core::frame::{NalUnits, avcc_to_annex_b, h264_nal_type, h265_nal_type};
use oxrtc_core::{Codec, CodecParams, Frame, MediaKind, TrackInfo};
use str0m::format::{Codec as RtcCodec, PayloadParams};
use str0m::media::{Frequency, MediaTime, Mid, Pt};

/// A negotiated sending media line.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) mid: Mid,
    pub(crate) kind: MediaKind,
    pub(crate) params: Vec<PayloadParams>,
}

fn rtc_codec(codec: Codec) -> Option<RtcCodec> {
    Some(match codec {
        Codec::H264 => RtcCodec::H264,
        Codec::H265 => RtcCodec::H265,
        Codec::Opus => RtcCodec::Opus,
        Codec::Pcma => RtcCodec::PCMA,
        Codec::Pcmu => RtcCodec::PCMU,
        _ => return None,
    })
}

/// The payload type to send `track` with on one of `outputs`. For H.264, a
/// non-interleaved type (packetization-mode 1) of the stream's own profile is
/// preferred; G.711 is only sent at 8 kHz.
pub(crate) fn choose(outputs: &[Output], track: &TrackInfo) -> Option<(Mid, Pt)> {
    let wanted = rtc_codec(track.codec)?;
    if matches!(track.codec, Codec::Pcma | Codec::Pcmu) && track.clock_rate != 8000 {
        return None;
    }
    let profile = match &track.params {
        CodecParams::H264 { sps, .. } => sps.get(1).copied(),
        _ => None,
    };
    let score = |p: &PayloadParams| {
        let spec = p.spec();
        let mode = u8::from(spec.format.packetization_mode == Some(1));
        let same_profile = spec.format.profile_level_id.map(|id| (id >> 16) as u8) == profile && profile.is_some();
        (mode, u8::from(same_profile))
    };
    outputs.iter().filter(|o| o.kind == track.kind).find_map(|o| {
        o.params.iter().filter(|p| p.spec().codec == wanted).max_by_key(|p| score(p)).map(|p| (o.mid, p.pt()))
    })
}

/// How one subscription track is sent.
#[derive(Debug)]
pub(crate) struct TrackOut {
    pub(crate) info: TrackInfo,
    pub(crate) target: Option<(Mid, Pt)>,
    need_keyframe: bool,
    /// The RTP time and wall clock of the last frame written.
    last_sent: Option<(u64, Instant)>,
    /// The step between its last two frames, in its clock.
    interval: u64,
    /// The timeline time of the last frame a replay sent again: the queued
    /// frames up to it are repeats ([`crate::replay`]).
    replayed_through: Option<i64>,
    /// A replay's times ran ahead of the frames' own: until a frame's own
    /// time passes the last one written, it is written just after it.
    restamped: bool,
}

impl TrackOut {
    pub(crate) fn new(info: TrackInfo, target: Option<(Mid, Pt)>) -> Self {
        // Until two frames are sent: 25 frames a second.
        let interval = u64::from(info.clock_rate / 25).max(1);
        Self { info, target, need_keyframe: true, last_sent: None, interval, replayed_through: None, restamped: false }
    }

    /// A frame was written at `time` (RTP), dated `wall`.
    pub(crate) fn sent(&mut self, time: u64, wall: Instant) {
        if let Some((last, _)) = self.last_sent
            && time > last
        {
            self.interval = time - last;
        }
        self.last_sent = Some((time, wall));
    }

    pub(crate) fn last_sent(&self) -> Option<(u64, Instant)> {
        self.last_sent
    }

    pub(crate) fn interval(&self) -> u64 {
        self.interval
    }

    /// A replay sent the cached frames up to `through` again (timeline
    /// time): video is decodable from here, and what was queued up to it is
    /// a repeat.
    pub(crate) fn replayed(&mut self, through: i64) {
        self.need_keyframe = false;
        self.replayed_through = Some(through);
        self.restamped = self.last_sent.is_some();
    }

    /// Whether a frame of time `pts` is one a replay already sent.
    pub(crate) fn repeats(&mut self, pts: i64) -> bool {
        match self.replayed_through {
            Some(through) if pts <= through => true,
            Some(_) => {
                self.replayed_through = None;
                false
            }
            None => false,
        }
    }

    fn is_video(&self) -> bool {
        self.info.kind == MediaKind::Video
    }

    /// Whether a frame can be sent: video waits for a keyframe after a start
    /// or a loss.
    pub(crate) fn admit(&mut self, keyframe: bool) -> bool {
        if self.target.is_none() {
            return false;
        }
        if self.is_video() && self.need_keyframe {
            if !keyframe {
                return false;
            }
            self.need_keyframe = false;
        }
        true
    }

    /// A frame was lost: video resumes at the next keyframe.
    pub(crate) fn lost(&mut self) {
        if self.is_video() {
            self.need_keyframe = true;
        }
    }

    /// The RTP time to write a new frame of time `pts` at: its own
    /// ([`TrackOut::rtp_time`]), except right after a replay. The replay's
    /// last time is set where the next frame is expected, one last step
    /// later; a camera's steps vary, and a frame that comes a little early
    /// would go back in time, which a browser drops. It is written one tick
    /// after the last time instead, until the frames' own times are ahead.
    pub(crate) fn time_of(&mut self, pts: i64) -> MediaTime {
        let own = u64::try_from(pts).unwrap_or(0);
        match self.last_sent {
            Some((last, _)) if self.restamped && own <= last => self.media_time(last.saturating_add(1)),
            _ => {
                self.restamped = false;
                self.media_time(own)
            }
        }
    }

    /// The RTP time of a frame: its timeline time, in the track's clock.
    pub(crate) fn rtp_time(&self, pts: i64) -> MediaTime {
        self.media_time(u64::try_from(pts).unwrap_or(0))
    }

    /// `ticks` of the track's clock.
    pub(crate) fn media_time(&self, ticks: u64) -> MediaTime {
        let clock = self.info.clock_rate.max(1);
        MediaTime::new(ticks, Frequency::new(clock).unwrap_or(Frequency::NINETY_KHZ))
    }

    /// The payload str0m expects: Annex B for H.264/H.265, with the
    /// parameter sets the source announced put before a keyframe that lacks
    /// them; other codecs as they are.
    pub(crate) fn payload(&self, frame: &Frame) -> Vec<u8> {
        match (self.info.codec, frame.keyframe) {
            (Codec::H264 | Codec::H265, keyframe) => annex_b(self.info.codec, &self.info.params, frame, keyframe),
            _ => frame.data.to_vec(),
        }
    }
}

/// The frame in Annex B (the core's conversion), after the announced
/// parameter sets when it is a keyframe that does not carry them.
fn annex_b(codec: Codec, params: &CodecParams, frame: &Frame, keyframe: bool) -> Vec<u8> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let body = avcc_to_annex_b(&frame.data);
    let sets: Vec<&[u8]> = match params {
        CodecParams::H264 { sps, pps } if keyframe && codec == Codec::H264 => vec![sps, pps],
        CodecParams::H265 { vps, sps, pps } if keyframe && codec == Codec::H265 => vec![vps, sps, pps],
        _ => Vec::new(),
    };
    let has_sets = || {
        NalUnits::new(&frame.data).any(|nal| match codec {
            Codec::H264 => h264_nal_type(nal) == Some(7),
            _ => h265_nal_type(nal) == Some(33),
        })
    };
    if sets.is_empty() || has_sets() {
        return Vec::from(body);
    }
    let mut out = Vec::with_capacity(body.len() + sets.iter().map(|s| s.len() + START.len()).sum::<usize>());
    for set in sets.into_iter().filter(|s| !s.is_empty()) {
        out.extend_from_slice(&START);
        out.extend_from_slice(set);
    }
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use oxrtc_core::frame::AnnexBUnits;

    use super::*;

    fn video(codec: Codec) -> TrackOut {
        TrackOut::new(TrackInfo::video(codec), Some((Mid::from("0"), Pt::from(96))))
    }

    fn frame(data: &'static [u8], keyframe: bool) -> Frame {
        Frame::new(0, 0, keyframe, Bytes::from_static(data))
    }

    #[test]
    fn video_waits_for_a_keyframe_and_again_after_a_loss() {
        let mut t = video(Codec::H264);
        assert!(!t.admit(false));
        assert!(t.admit(true));
        assert!(t.admit(false));
        t.lost();
        assert!(!t.admit(false));
        assert!(t.admit(true));
    }

    #[test]
    fn audio_is_never_held_back_and_unsendable_tracks_are_skipped() {
        let mut a = TrackOut::new(TrackInfo::audio(Codec::Opus, 48_000, 2), Some((Mid::from("1"), Pt::from(111))));
        a.lost();
        assert!(a.admit(false));
        let mut none = TrackOut::new(TrackInfo::video(Codec::H264), None);
        assert!(!none.admit(true));
    }

    #[test]
    fn h264_keyframe_gets_its_parameter_sets() {
        let sps = Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e]);
        let pps = Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80]);
        let mut t = video(Codec::H264);
        t.info.params = CodecParams::H264 { sps: sps.clone(), pps: pps.clone() };
        let out = t.payload(&frame(&[0, 0, 0, 3, 0x65, 0x88, 0x84], true));
        let units: Vec<&[u8]> = AnnexBUnits::new(&out).collect();
        assert_eq!(units, vec![&sps[..], &pps[..], &[0x65, 0x88, 0x84][..]]);
        let delta = t.payload(&frame(&[0, 0, 0, 2, 0x41, 0x9a], false));
        assert_eq!(delta, vec![0, 0, 0, 1, 0x41, 0x9a]);
    }

    #[test]
    fn present_parameter_sets_are_not_doubled() {
        let mut t = video(Codec::H264);
        t.info.params = CodecParams::H264 { sps: Bytes::from_static(&[0x67, 1]), pps: Bytes::from_static(&[0x68, 2]) };
        let out = t.payload(&frame(&[0, 0, 0, 2, 0x67, 9, 0, 0, 0, 2, 0x68, 9, 0, 0, 0, 2, 0x65, 1], true));
        assert_eq!(AnnexBUnits::new(&out).count(), 3);
    }

    #[test]
    fn h265_keyframe_gets_vps_sps_pps() {
        let mut t = video(Codec::H265);
        t.info.params = CodecParams::H265 {
            vps: Bytes::from_static(&[0x40, 1]),
            sps: Bytes::from_static(&[0x42, 1]),
            pps: Bytes::from_static(&[0x44, 1]),
        };
        let out = t.payload(&frame(&[0, 0, 0, 3, 0x26, 0x01, 0xaf], true));
        let types: Vec<u8> = AnnexBUnits::new(&out).filter_map(h265_nal_type).collect();
        assert_eq!(types, vec![32, 33, 34, 19]);
    }

    #[test]
    fn a_replay_makes_video_decodable_and_its_repeats_are_known() {
        let mut t = video(Codec::H264);
        t.lost();
        t.replayed(36_000);
        assert!(t.admit(false), "the replay began at a keyframe");
        assert!(t.repeats(32_400) && t.repeats(36_000), "queued frames the replay sent again");
        assert!(!t.repeats(39_600), "the first new frame");
        assert!(!t.repeats(36_000), "and the repeats end with it");
    }

    #[test]
    fn a_frame_after_a_replay_never_goes_back_in_time() {
        let mut t = video(Codec::H264);
        let now = Instant::now();
        t.sent(360_000, now);
        // A replay of 50 frames, caught up: stamped up to where the next
        // frame is expected, one step (3600) later.
        for time in crate::replay::stamps(360_000, 360_000, 3600, 50).unwrap() {
            t.sent(time, now);
        }
        t.replayed(360_000);
        let (last, _) = t.last_sent().unwrap();
        // The camera's next frame comes a tenth of a step early.
        assert!(!t.repeats(363_240));
        let next = t.time_of(363_240).numer();
        assert!(next > last, "{next} is back from the replay's {last}");
        t.sent(next, now);
        // The one after it is ahead again: its own time.
        assert_eq!(t.time_of(366_840).numer(), 366_840);
        t.sent(366_840, now);
        assert_eq!(t.time_of(370_440).numer(), 370_440, "only a replay moves a time");
    }

    #[test]
    fn the_interval_follows_the_frames_written() {
        let mut t = video(Codec::H264);
        assert_eq!(t.interval(), 3600, "25 frames a second until known");
        let now = Instant::now();
        t.sent(9000, now);
        t.sent(10_500, now);
        assert_eq!((t.interval(), t.last_sent()), (1500, Some((10_500, now))));
        t.sent(10_500, now);
        assert_eq!(t.interval(), 1500, "a repeated time is no step");
    }

    #[test]
    fn rtp_time_is_the_hubs_timeline_as_it_is() {
        // No continuity logic of its own: the core's timeline already has it.
        let a = TrackOut::new(TrackInfo::audio(Codec::Opus, 48_000, 2), None);
        let time = a.rtp_time(4_800_000_123);
        assert_eq!((time.numer(), time.frequency().get()), (4_800_000_123, 48_000));
        let v = video(Codec::H264);
        assert_eq!(v.rtp_time(5).numer(), 5, "a small time stays where it is");
        assert_eq!(v.rtp_time(-1).numer(), 0, "never negative on the timeline; refused here all the same");
    }
}
