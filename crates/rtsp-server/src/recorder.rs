//! A publishing session: `ANNOUNCE` on a registered token, `SETUP` with
//! `mode=record`, `RECORD`, then RTP depacketized into the token's sink.

use std::net::SocketAddr;

use oxrtc_core::{CancellationToken, Frame};
use rtsp_types::StatusCode;
use rtsp_types::headers::{SESSION, TRANSPORT};
use tokio::time::Instant;

use crate::Shared;
use crate::conn::{Reply, Req, interleaved, is_loopback, target};
use crate::depacketize::{Depacketized, TrackDepacketizer};
use crate::publish::{Claim, PUBLISH_PREFIX};
use crate::sdp::{Announced, parse_announce};
use crate::wire::reply;

struct Track {
    announced: Announced,
    depacketizer: TrackDepacketizer,
    /// Interleaved RTP channel, once set up.
    channel: Option<u8>,
}

/// A publishing session.
pub(crate) struct Recorder {
    claim: Claim,
    tracks: Vec<Track>,
    /// Indexes into `tracks`, in `SETUP` order: the published track list.
    published: Vec<usize>,
    recording: bool,
    deadline: Option<Instant>,
    scratch: Vec<Depacketized>,
    errors: u64,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.errors > 0 {
            tracing::debug!(errors = self.errors, "rtsp: publication had depacketization errors");
        }
    }
}

impl Recorder {
    /// Takes a registered token from a loopback peer and reads its SDP.
    pub(crate) fn announce(shared: &Shared, peer: SocketAddr, request: &Req) -> Result<(Self, Reply), Reply> {
        let fail = |status| reply(request, status).build(Vec::new());
        let (path, _) = target(request).ok_or_else(|| fail(StatusCode::BadRequest))?;
        let Some(token) = path.strip_prefix(PUBLISH_PREFIX) else { return Err(fail(StatusCode::Forbidden)) };
        if !is_loopback(peer.ip()) {
            return Err(fail(StatusCode::Forbidden));
        }
        let limits = shared.hub.limits();
        let announced = parse_announce(request.body(), limits.max_tracks_per_source).map_err(|error| {
            tracing::debug!(%error, "rtsp: refused announce");
            fail(StatusCode::BadRequest)
        })?;
        let tracks: Vec<Track> = announced
            .into_iter()
            .filter_map(|announced| {
                let depacketizer =
                    TrackDepacketizer::new(announced.spec.clone(), announced.info.codec, limits.max_frame_bytes)
                        .ok()?;
                Some(Track { announced, depacketizer, channel: None })
            })
            .collect();
        if tracks.is_empty() {
            return Err(fail(StatusCode::UnsupportedMediaType));
        }
        // Only once the request is known good: claiming ends the previous session.
        let claim = shared.publishers.claim(token).ok_or_else(|| fail(StatusCode::NotFound))?;
        let recorder = Self {
            claim,
            tracks,
            published: Vec::new(),
            recording: false,
            deadline: Instant::now().checked_add(shared.config.setup_play_timeout),
            scratch: Vec::new(),
            errors: 0,
        };
        Ok((recorder, reply(request, StatusCode::Ok).build(Vec::new())))
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Cancelled when the token is deregistered or claimed by a newer session.
    pub(crate) fn session_token(&self) -> CancellationToken {
        self.claim.session.clone()
    }

    fn find(&self, uri: &str) -> Option<usize> {
        let by_control = self.tracks.iter().position(|t| {
            t.announced
                .control
                .as_deref()
                .is_some_and(|c| !c.is_empty() && (uri == c || uri.ends_with(&format!("/{c}"))))
        });
        by_control.or_else(|| self.tracks.iter().position(|t| t.channel.is_none()))
    }

    pub(crate) fn setup(&mut self, request: &Req, session: &str) -> Reply {
        let fail = |status| reply(request, status).build(Vec::new());
        if self.recording {
            return fail(StatusCode::MethodNotValidInThisState);
        }
        let Some(transport) = interleaved(request) else { return fail(StatusCode::UnsupportedTransport) };
        let uri = request.request_uri().map(|u| u.as_str().to_owned()).unwrap_or_default();
        let Some(index) = self.find(&uri) else { return fail(StatusCode::NotFound) };
        let channels = transport.channels.unwrap_or_else(|| {
            let rtp = u8::try_from(self.published.len() * 2).unwrap_or(0);
            (rtp, rtp.wrapping_add(1))
        });
        if self.tracks.iter().enumerate().any(|(i, t)| i != index && t.channel == Some(channels.0)) {
            return fail(StatusCode::BadRequest);
        }
        let Some(track) = self.tracks.get_mut(index) else { return fail(StatusCode::NotFound) };
        if track.channel.replace(channels.0).is_none() {
            self.published.push(index);
        }
        let header = format!("RTP/AVP/TCP;unicast;interleaved={}-{};mode=record", channels.0, channels.1);
        reply(request, StatusCode::Ok).header(TRANSPORT, header).header(SESSION, session).build(Vec::new())
    }

    pub(crate) fn record(&mut self, request: &Req, session: &str) -> Reply {
        let fail = |status| reply(request, status).build(Vec::new());
        if self.published.is_empty() {
            return fail(StatusCode::MethodNotValidInThisState);
        }
        let infos = self
            .published
            .iter()
            .filter_map(|&i| self.tracks.get(i))
            .map(|t| t.announced.info.clone().with_params(t.depacketizer.params()))
            .collect();
        if let Err(error) = self.claim.sink.set_tracks(infos) {
            tracing::debug!(%error, "rtsp: publication tracks refused");
            return fail(StatusCode::BadRequest);
        }
        self.recording = true;
        self.deadline = None;
        reply(request, StatusCode::Ok).header(SESSION, session).build(Vec::new())
    }

    /// One interleaved packet. RTCP and unknown channels are ignored.
    pub(crate) fn on_data(&mut self, channel: u8, packet: &[u8]) {
        if !self.recording {
            return;
        }
        let Some(position) =
            self.published.iter().position(|&i| self.tracks.get(i).and_then(|t| t.channel) == Some(channel))
        else {
            return;
        };
        let Some(track) = self.published.get(position).and_then(|&i| self.tracks.get_mut(i)) else { return };
        let Ok(track_id) = u8::try_from(position) else { return };
        self.scratch.clear();
        if track.depacketizer.push(packet, &mut self.scratch).is_err() {
            self.errors += 1;
        }
        for frame in self.scratch.drain(..) {
            if self.claim.sink.publish(Frame::new(track_id, frame.pts, frame.keyframe, frame.data)).is_err() {
                self.errors += 1;
            }
        }
    }
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
