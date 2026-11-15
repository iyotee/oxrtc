//! The cap on FFmpeg processes running at once (`ffmpeg.max_processes`).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use oxrtc_core::SourceError;

/// The cap on FFmpeg processes running at once.
#[derive(Debug, Clone)]
pub(crate) struct Limiter {
    permits: Arc<Semaphore>,
    max: usize,
}

impl Limiter {
    pub(crate) fn new(max: usize) -> Self {
        let max = max.clamp(1, Semaphore::MAX_PERMITS);
        Self { permits: Arc::new(Semaphore::new(max)), max }
    }

    /// A permit for one process, or the reason there is none.
    pub(crate) fn try_acquire(&self) -> Result<OwnedSemaphorePermit, SourceError> {
        self.permits.clone().try_acquire_owned().map_err(|_| {
            SourceError::Unreachable(format!(
                "ffmpeg: {} processes already running, the limit (ffmpeg max_processes)",
                self.max
            ))
        })
    }

    /// A permit, waiting for one.
    pub(crate) async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().acquire_owned().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_cap_is_enforced_and_released() {
        let limiter = Limiter::new(2);
        let a = limiter.try_acquire().unwrap();
        let _b = limiter.try_acquire().unwrap();
        let refused = limiter.try_acquire().unwrap_err();
        assert!(refused.to_string().contains("max_processes"), "{refused}");
        assert!(refused.is_retryable(), "the core retries once a process ends");
        drop(a);
        assert!(limiter.try_acquire().is_ok());
        // Zero would forbid every process: at least one is allowed.
        assert!(Limiter::new(0).try_acquire().is_ok());
    }
}
