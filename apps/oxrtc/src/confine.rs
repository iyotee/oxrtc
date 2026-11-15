//! FFmpeg's confinement at startup (D5): on Linux, `oxrtc-exec` beside this
//! binary, checked once, and what holds said once in the log.

use std::path::Path;

use oxrtc_ffmpeg::{Confinement, Discovery, Landlock, Limits};

/// `ffmpeg.max_memory_mib` applied to FFmpeg's limits.
pub fn with_memory(mut limits: Limits, max_memory_mib: Option<usize>) -> Limits {
    if let Some(mib) = max_memory_mib {
        let mib = u64::try_from(mib).unwrap_or(u64::MAX);
        limits.address_space = Some(mib.saturating_mul(1024 * 1024));
    }
    limits
}

/// The confinement every FFmpeg of `bin` gets, or `None` to start it
/// directly (not Linux, or `oxrtc-exec` missing: then said loudly).
pub async fn ffmpeg(bin: &Path, max_memory_mib: Option<usize>) -> Option<Confinement> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!("ffmpeg: own path unknown ({error}): FFmpeg runs unconfined");
            return None;
        }
    };
    let mut confinement = match Confinement::discover(&exe) {
        Discovery::NotHere => return None,
        Discovery::Missing(wrapper) => {
            tracing::warn!(
                "ffmpeg: {} not found: FFmpeg runs unconfined, and outlives oxrtc if oxrtc is killed",
                wrapper.display()
            );
            return None;
        }
        Discovery::Found(confinement) => confinement,
    };
    confinement.limits = with_memory(confinement.limits, max_memory_mib);
    let limits = confinement.limits;
    let memory_mib = limits.address_space.map_or(0, |bytes| bytes / (1024 * 1024));
    match confinement.check(bin).await {
        Ok(state @ Landlock::Full) => tracing::info!(
            memory_mib,
            nice = limits.nice,
            "ffmpeg: confined (dies with oxrtc, no new privileges, limits, {})",
            state.summary()
        ),
        Ok(state @ Landlock::Partial { .. }) => tracing::info!(
            memory_mib,
            nice = limits.nice,
            "ffmpeg: confined (dies with oxrtc, no new privileges, limits, {}){}",
            state.summary(),
            not_enforced(state)
        ),
        Ok(Landlock::None) => tracing::warn!(
            memory_mib,
            nice = limits.nice,
            "ffmpeg: dies with oxrtc and is limited, but this kernel has no Landlock: its file system is not restricted"
        ),
        // Every FFmpeg will be refused the same way, and say so as its
        // source's error: confined or not at all, never quietly unconfined.
        Err(error) => {
            tracing::error!("ffmpeg: oxrtc-exec refuses to start FFmpeg: {}", oxrtc_core::spec::redact(&error))
        }
    }
    Some(confinement)
}

/// What a partial ruleset leaves out on this kernel, as the end of the log
/// line (empty when nothing FFmpeg's ruleset relies on is missing).
fn not_enforced(state: Landlock) -> String {
    let gaps = state.unenforced();
    if gaps.is_empty() { String::new() } else { format!("; not enforced by this kernel: {}", gaps.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_ruleset_is_logged_with_what_it_leaves_out() {
        let line = not_enforced(Landlock::Partial { abi: 4 });
        assert!(line.starts_with("; not enforced by this kernel: "), "{line}");
        assert!(line.contains("signal and abstract-socket scoping"), "{line}");
        assert_eq!(not_enforced(Landlock::Partial { abi: 6 }), "");
        assert_eq!(not_enforced(Landlock::Full), "");
    }

    #[test]
    fn the_memory_key_replaces_the_address_space_only() {
        let defaults = oxrtc_ffmpeg::FFMPEG_LIMITS;
        assert_eq!(with_memory(defaults, None), defaults);
        let limits = with_memory(defaults, Some(1536));
        assert_eq!(limits.address_space, Some(1536 * 1024 * 1024));
        assert_eq!(Limits { address_space: defaults.address_space, ..limits }, defaults);
        assert_eq!(with_memory(defaults, Some(usize::MAX)).address_space, Some(u64::MAX));
    }

    /// Here (Windows in development) FFmpeg starts directly; on Linux the
    /// test binary has no `oxrtc-exec` beside it, so directly too, with a
    /// warning.
    #[tokio::test]
    async fn without_a_wrapper_ffmpeg_starts_directly() {
        assert_eq!(ffmpeg(Path::new("ffmpeg"), None).await, None);
    }
}
