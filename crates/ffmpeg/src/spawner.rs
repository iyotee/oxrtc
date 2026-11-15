//! Every FFmpeg is started from one thread that lives as long as oxrtc.
//!
//! On Linux the death signal `oxrtc-exec` arms (`PR_SET_PDEATHSIG`) fires
//! when the *thread* that started it ends, not the process (prctl(2)). Started
//! from whichever runtime thread polled the run, an FFmpeg would be killed the
//! day that thread ends - a blocking-pool thread idles out after 10 s, and
//! `block_in_place` hands a worker's role to another thread. This thread never
//! ends, so FFmpeg dies with oxrtc and only then. The same path serves every
//! platform, so the tests on Windows exercise it too.

use std::io;
use std::sync::OnceLock;

use tokio::process::{Child, Command};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

/// Starts waiting at once: more than the process caps let run together
/// (`max_processes` and the stills'), never reached in practice.
const QUEUE: usize = 256;

struct Request {
    command: Command,
    /// The caller's runtime: the child is registered with it, to be awaited
    /// and reaped there.
    runtime: Handle,
    reply: oneshot::Sender<io::Result<Child>>,
}

static THREAD: OnceLock<Option<mpsc::Sender<Request>>> = OnceLock::new();

fn thread() -> Option<&'static mpsc::Sender<Request>> {
    THREAD
        .get_or_init(|| {
            let (sender, mut requests) = mpsc::channel::<Request>(QUEUE);
            std::thread::Builder::new()
                .name("oxrtc-ffmpeg-start".into())
                .spawn(move || {
                    // Blocks outside any runtime: the runtime is entered for
                    // one start only.
                    while let Some(mut request) = requests.blocking_recv() {
                        let started = {
                            let _runtime = request.runtime.enter();
                            request.command.spawn()
                        };
                        let _ = request.reply.send(started);
                    }
                })
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Starts `command` on the starting thread; on the calling thread only if
/// that thread could not be created (then logged: the death signal may come
/// early).
pub(crate) async fn spawn(mut command: Command) -> io::Result<Child> {
    let (Some(thread), Ok(runtime)) = (thread(), Handle::try_current()) else {
        tracing::warn!("ffmpeg: no starting thread, started from a runtime thread");
        return command.spawn();
    };
    let (reply, started) = oneshot::channel();
    if let Err(refused) = thread.send(Request { command, runtime, reply }).await {
        let mut command = refused.0.command;
        tracing::warn!("ffmpeg: the starting thread is gone, started from a runtime thread");
        return command.spawn();
    }
    started.await.unwrap_or_else(|_| Err(io::Error::other("the starting thread dropped the start")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_exit() -> Command {
        let mut command = if cfg!(windows) { Command::new("cmd") } else { Command::new("true") };
        if cfg!(windows) {
            command.args(["/c", "exit 0"]);
        }
        command
    }

    #[tokio::test]
    async fn a_start_happens_on_the_starting_thread_and_is_awaited_here() {
        let mut child = spawn(quick_exit()).await.unwrap();
        assert!(child.wait().await.unwrap().success());
        assert!(thread().is_some());
    }

    /// Runtimes come and go (each test has its own): a start always uses
    /// the caller's.
    #[test]
    fn starts_from_runtimes_that_came_and_went() {
        for _ in 0..3 {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let mut child = spawn(quick_exit()).await.unwrap();
                assert!(child.wait().await.unwrap().success());
            });
        }
    }

    #[tokio::test]
    async fn a_failed_start_says_why() {
        let error = spawn(Command::new("/nonexistent/ffmpeg")).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
