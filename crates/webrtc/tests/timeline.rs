//! R20 for WebRTC: a viewer across a source whose timestamps start again. The
//! RTP time it receives is the hub's timeline, the RTSP egress's too; this
//! egress has no continuity logic of its own any more (it used to close the
//! gap of a restart to 40 ms, track by track, which parted picture and sound
//! by however long the source was away).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ClientEvent, ScriptFrame, browser, connect, opus_frames, server, synthetic_h264};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Drivers, FactoryCtx, Frame, Hub, Limits, ProducerSink, SourceDriver,
    SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};
use str0m::media::MediaKind as RtcKind;

/// H.264 at 25 frames a second and Opus every 20 ms, from pts 0 at each run;
/// a run is one GOP (1 s), then it ends and the hub starts it again.
struct Restarting {
    tracks: Vec<TrackInfo>,
    video: Vec<ScriptFrame>,
}

impl SourceDriver for Restarting {
    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            sink.set_tracks(self.tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            let mut ticker = tokio::time::interval(Duration::from_millis(40));
            let sound = opus_frames(1, 2);
            for (n, picture) in self.video.iter().enumerate() {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    _ = ticker.tick() => {}
                }
                let n = n as i64;
                let publish = |frame| sink.publish(frame).map_err(|e| SourceError::Refused(e.to_string()));
                publish(Frame::new(0, n * 3600, picture.keyframe, picture.data.clone()))?;
                for (half, f) in sound.iter().enumerate() {
                    publish(Frame::new(1, (2 * n + half as i64) * 960, true, f.data.clone()))?;
                }
            }
            Ok(())
        })
    }
}

struct Factory(Arc<Restarting>);

impl SourceFactory for Factory {
    fn schemes(&self) -> &[&'static str] {
        &["restart"]
    }

    fn build(&self, _spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        Ok(self.0.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_starts_its_timestamps_again_keeps_the_viewers_clock_on_the_wall() {
    let (params, video) = synthetic_h264();
    let tracks = vec![TrackInfo::video(Codec::H264).with_params(params), TrackInfo::audio(Codec::Opus, 48_000, 2)];
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(Factory(Arc::new(Restarting { tracks, video }))));
    // Away about half a second at each restart: a gap the old per-session
    // logic closed to 40 ms.
    let limits = Limits {
        backoff_initial: Duration::from_millis(450),
        backoff_max: Duration::from_millis(500),
        ..Limits::default()
    };
    let hub = Hub::new(drivers, limits);
    let stream = hub.upsert("cam", &["restart:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), Duration::from_secs(10)).await;
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    let mut seen: [Vec<(u64, std::time::Instant)>; 2] = [Vec::new(), Vec::new()];
    let end = tokio::time::Instant::now() + Duration::from_millis(4500);
    while let Some(event) = client.wait_for(end - tokio::time::Instant::now(), |_| true).await {
        if let ClientEvent::Media { kind, time, at, .. } = event {
            seen[usize::from(kind == RtcKind::Audio)].push((time, at));
        }
    }
    for (name, rate, frames) in [("video", 90_000.0, &seen[0]), ("sound", 48_000.0, &seen[1])] {
        assert!(frames.len() > 40, "{name}: {} frames", frames.len());
        let mut gaps = 0;
        for pair in frames.windows(2) {
            let media = f64::from((pair[1].0 as u32).wrapping_sub(pair[0].0 as u32) as i32) / rate;
            let wall = pair[1].1.duration_since(pair[0].1).as_secs_f64();
            assert!((media - wall).abs() < 0.2, "{name}: {media:+.3} s of RTP time over {wall:.3} s");
            gaps += usize::from(wall > 0.3);
        }
        assert!(gaps >= 2, "{name}: {gaps} restarts seen");
    }
}
