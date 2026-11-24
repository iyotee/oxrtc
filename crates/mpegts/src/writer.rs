//! The writer: tracks in, the program's tables, frames in, packets out.

use bytes::Bytes;
use oxrtc_core::{Frame, TrackInfo};

use crate::es::Es;
use crate::packet::{Marks, Pid};
use crate::pes::{self, TIME_MASK};
use crate::psi::{self, Entry};
use crate::{CLOCK, FIRST_PID, MAX_TRACKS, PMT_PID, TIME_OFFSET, TsError};

/// Whether [`Writer::new`] takes this track: H.264 and H.265 (with or
/// without their parameter sets: frames carry them in band), AAC whose
/// configuration an ADTS header can say (object types 1 to 4 of the core
/// coder, a rate of the table, a layout by number), Opus of one or two
/// channels at 48 kHz. Not G.711, L16 nor Motion JPEG.
pub fn supported(track: &TrackInfo) -> bool {
    Es::of(track).is_ok()
}

#[derive(Debug, Clone)]
struct Track {
    es: Es,
    pid: Pid,
    stream_id: u8,
    clock: u32,
}

/// An MPEG-TS writer for one track list: one program, a PID per track from
/// [`FIRST_PID`]. A new list (a source that came back with other tracks)
/// takes a new writer.
#[derive(Debug, Clone)]
pub struct Writer {
    tracks: Vec<Track>,
    /// The track whose PES carry the program clock reference.
    pcr: usize,
    pat: Pid,
    pmt: Pid,
    pat_section: Vec<u8>,
    pmt_section: Vec<u8>,
}

/// `ticks` of `clock` on the 90 kHz clock.
fn to_90k(ticks: i64, clock: u32) -> Result<u64, TsError> {
    let scaled = i128::from(ticks) * i128::from(CLOCK) / i128::from(clock.max(1));
    u64::try_from(scaled).map_err(|_| TsError::Time("a negative time"))
}

impl Writer {
    /// A writer for `tracks`, each [`supported`].
    pub fn new(tracks: &[TrackInfo]) -> Result<Self, TsError> {
        if tracks.is_empty() || tracks.len() > MAX_TRACKS {
            return Err(TsError::TrackCount { count: tracks.len() });
        }
        let mut built = Vec::with_capacity(tracks.len());
        let mut entries = Vec::with_capacity(tracks.len());
        let (mut video, mut audio) = (0u8, 0u8);
        for (pid, info) in (FIRST_PID..).zip(tracks) {
            if info.clock_rate == 0 {
                return Err(TsError::Unsupported("a clock rate of 0"));
            }
            let es = Es::of(info)?;
            let (stream_type, descriptors) = es.stream_type(info.channels);
            let stream_id = match &es {
                Es::Opus => pes::PRIVATE_1,
                es if es.video() => {
                    video += 1;
                    pes::VIDEO + video - 1
                }
                _ => {
                    audio += 1;
                    pes::AUDIO + audio - 1
                }
            };
            entries.push(Entry { stream_type, pid, descriptors });
            built.push(Track { es, pid: Pid::new(pid), stream_id, clock: info.clock_rate });
        }
        let pcr = built.iter().position(|t| t.es.video()).unwrap_or(0);
        let pcr_pid = FIRST_PID + u16::try_from(pcr).unwrap_or(0);
        Ok(Self {
            tracks: built,
            pcr,
            pat: Pid::new(0),
            pmt: Pid::new(PMT_PID),
            pat_section: psi::pat(PMT_PID),
            pmt_section: psi::pmt(pcr_pid, &entries),
        })
    }

    /// The PID of each track, in track order.
    pub fn pids(&self) -> Vec<u16> {
        self.tracks.iter().map(|t| t.pid.pid).collect()
    }

    /// The PAT and the PMT, one packet each: what starts a segment, so that
    /// a player can begin there.
    pub fn tables(&mut self) -> Bytes {
        let mut out = Vec::with_capacity(2 * crate::PACKET_SIZE);
        self.pat.section(&mut out, &self.pat_section);
        self.pmt.section(&mut out, &self.pmt_section);
        Bytes::from(out)
    }

    /// The packets of a frame of track `track`, decoded at its presentation
    /// time. A refused frame changes nothing.
    pub fn push(&mut self, track: usize, frame: &Frame) -> Result<Bytes, TsError> {
        self.push_decoded(track, frame, frame.pts)
    }

    /// [`Writer::push`] for a frame whose decode time `dts` differs from its
    /// presentation time (B-frames). Times are in the track's clock, never
    /// negative; they wrap at 33 bits on the 90 kHz clock (26.5 hours), as
    /// every MPEG-TS reader expects.
    pub fn push_decoded(&mut self, track: usize, frame: &Frame, dts: i64) -> Result<Bytes, TsError> {
        let pcr_track = self.pcr;
        let state = self.tracks.get_mut(track).ok_or(TsError::NoTrack(track))?;
        let pts90 = to_90k(frame.pts, state.clock)?;
        let dts90 = to_90k(dts, state.clock)?;
        if dts90 > pts90 {
            return Err(TsError::Time("a decode time after the presentation time"));
        }
        let keyframe = !state.es.video() || frame.keyframe;
        let payload = state.es.write(&frame.data, keyframe)?;
        let video = state.es.video();
        let pts = (pts90 + TIME_OFFSET) & TIME_MASK;
        let dts = (dts90 + TIME_OFFSET) & TIME_MASK;
        let mut out = pes::header(state.stream_id, pts, Some(dts), payload.len(), !video)?;
        out.extend_from_slice(&payload);
        let marks = Marks {
            pcr: (track == pcr_track).then_some(dts90 & TIME_MASK),
            // A picture a decoder can start at; every sound frame of a
            // stream with no picture.
            random_access: if video { frame.keyframe } else { track == pcr_track },
        };
        let packets = out.len().div_ceil(crate::PACKET_SIZE - 4) + 1;
        let mut bytes = Vec::with_capacity(packets * crate::PACKET_SIZE);
        state.pid.pes(&mut bytes, &out, marks);
        Ok(Bytes::from(bytes))
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
