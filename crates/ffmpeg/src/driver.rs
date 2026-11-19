//! The `ffmpeg:` source: one FFmpeg process per run, publishing its output
//! back into oxrtc's own RTSP server (go2rtc's `-f rtsp {output}`).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{
    BoxFuture, CancellationToken, Declared, FactoryCtx, ProducerSink, SourceDriver, SourceError, SourceFactory,
    SourceSpec, SpecError,
};
use tokio::time::Instant;

use crate::args::{Plan, plan};
use crate::limiter::Limiter;
use crate::process::{Launch, Process, Tail, command, drain, spawn};
use crate::templates::Templates;
use crate::{FfmpegConfig, PublishTarget};

/// How often a starting run checks whether the publication arrived.
const START_POLL: Duration = Duration::from_millis(50);

/// How long the error output of an exited process is still read.
const LAST_WORDS: Duration = Duration::from_millis(500);

/// How long a killed process is given to be reaped.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// Builds `ffmpeg:` drivers. Every driver it builds shares one process cap.
pub struct FfmpegFactory {
    launch: Launch,
    relay: String,
    start_timeout: Duration,
    templates: Templates,
    limiter: Limiter,
    target: Arc<dyn PublishTarget>,
}

impl std::fmt::Debug for FfmpegFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegFactory")
            .field("launch", &self.launch)
            .field("relay", &self.relay)
            .finish_non_exhaustive()
    }
}

impl FfmpegFactory {
    /// A factory running `config.bin`, publishing through `target`.
    pub fn new(config: FfmpegConfig, target: Arc<dyn PublishTarget>) -> Self {
        Self {
            templates: Templates::new(&config.templates),
            limiter: Limiter::new(config.max_processes),
            launch: Launch::new(&config),
            relay: config.relay,
            start_timeout: config.start_timeout,
            target,
        }
    }
}

impl SourceFactory for FfmpegFactory {
    fn schemes(&self) -> &[&'static str] {
        &["ffmpeg"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let plan = plan(spec, &self.templates, &self.relay, ctx.stream_name())?;
        Ok(Arc::new(FfmpegDriver {
            label: spec.redacted(),
            plan,
            launch: self.launch.clone(),
            start_timeout: self.start_timeout,
            limiter: self.limiter.clone(),
            target: self.target.clone(),
        }))
    }
}

struct FfmpegDriver {
    label: String,
    plan: Plan,
    launch: Launch,
    start_timeout: Duration,
    limiter: Limiter,
    target: Arc<dyn PublishTarget>,
}

impl SourceDriver for FfmpegDriver {
    fn declared(&self) -> Option<Declared> {
        self.plan.declared()
    }

    fn stall_timeout(&self) -> Option<Duration> {
        self.plan.stall_timeout()
    }

    fn start_timeout(&self) -> Option<Duration> {
        Some(self.start_timeout)
    }

    fn local(&self) -> bool {
        self.plan.local()
    }

    fn transcodes(&self) -> bool {
        true
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move { self.run_once(sink, cancel).await })
    }
}

impl FfmpegDriver {
    async fn run_once(&self, sink: ProducerSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let _permit = self.limiter.try_acquire()?;
        // The lease lives as long as the run: dropping it ends the publication.
        let lease = self.target.lease(sink.clone());
        let credential = lease.read_credential();
        let args = self.plan.args_reading_as(&lease.url(), credential.as_ref().map(|(u, p)| (u.as_str(), p.as_str())));
        tracing::debug!(source = %self.label, "ffmpeg: starting");
        // Dropping `process` (the run cancelled or dropped by the core) kills
        // FFmpeg and whatever it started.
        let mut process = spawn(command(&self.launch, &args, Stdio::null(), Stdio::null()), &self.launch.bin)
            .await
            .map_err(|e| SourceError::Unreachable(format!("ffmpeg: {e}")))?;
        let tail = Tail::default();
        let stderr = process.child.stderr.take();
        let label = self.label.clone();
        let drain_tail = tail.clone();
        let drained = async move {
            if let Some(stderr) = stderr {
                drain(stderr, drain_tail, &label).await;
            }
        };
        tokio::pin!(drained);
        let mut drain_done = false;

        let deadline = Instant::now() + self.start_timeout;
        let mut started = false;
        let mut poll = tokio::time::interval(START_POLL);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    stop(&mut process).await;
                    return Ok(());
                }
                status = process.child.wait() => {
                    // The last lines say why: let them arrive.
                    if !drain_done {
                        let _ = tokio::time::timeout(LAST_WORDS, &mut drained).await;
                    }
                    return Err(exited(status, &tail));
                }
                () = &mut drained, if !drain_done => drain_done = true,
                _ = poll.tick(), if !started => {
                    if sink.tracks().is_some() {
                        started = true;
                        tracing::debug!(source = %self.label, "ffmpeg: publishing");
                    } else if Instant::now() >= deadline {
                        stop(&mut process).await;
                        let why = tail.joined().map(|l| format!(" ({l})")).unwrap_or_default();
                        return Err(SourceError::Unreachable(format!(
                            "ffmpeg: no publication within {:?}{why}",
                            self.start_timeout
                        )));
                    }
                }
            }
        }
    }
}

/// Kills the process and what it started, and reaps it, briefly.
async fn stop(process: &mut Process) {
    process.kill();
    let _ = tokio::time::timeout(REAP_GRACE, process.child.wait()).await;
}

/// Why a run ended when FFmpeg exited by itself.
fn exited(status: std::io::Result<std::process::ExitStatus>, tail: &Tail) -> SourceError {
    let status = match status {
        Ok(status) => status.to_string(),
        Err(error) => error.to_string(),
    };
    let joined = tail.joined();
    let why = joined.as_deref().map(|l| format!(": {l}")).unwrap_or_default();
    let text = format!("ffmpeg exited ({status}){why}");
    if joined.as_deref().is_some_and(declined_by_input) {
        SourceError::Declined(text)
    } else {
        SourceError::Unreachable(text)
    }
}

/// Whether FFmpeg's last words say its input answered with a client error:
/// `Server returned 404 Not Found`, `Server returned 4XX Client Error, but not
/// one of 40{0,1,3,4}` (a 415 of openIVR's sound de-duplicator, a 455 of an
/// RTSP server asked to play a track the stream has not). A 5XX is left out:
/// a relay answers 503 while its camera is still starting, and that one is
/// worth asking again soon.
fn declined_by_input(tail: &str) -> bool {
    tail.contains("Server returned 4")
}
