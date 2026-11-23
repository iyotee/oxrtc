//! What memory the host gives this process (M5 item D (6), D45): the readers
//! the process-wide output budget ([`crate::memory`]) is sized from.
//!
//! On Linux, a control group's limit when one is set (cgroup v2
//! `memory.max`, else v1 `memory.limit_in_bytes`: Docker on the Raspberry Pi)
//! with what the group uses (`memory.current`, `memory.usage_in_bytes`),
//! and `/proc/meminfo`'s `MemTotal` and `MemAvailable` otherwise and beside
//! it: what is available is the smaller of the two. On another system the
//! application gives its own [`HostMemory`] (the physical memory). The
//! parsers take the files' text and are fuzzed through their proptest twins;
//! [`LinuxMemory`] reads them under a root that a test points at recorded
//! files.

use std::path::PathBuf;

/// One reading of the memory the process may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostReading {
    /// The most it may ever use: the control group's limit, else the
    /// machine's memory.
    pub limit: u64,
    /// What it may still take now: under the limit and free on the machine.
    pub available: u64,
    /// Where the limit came from.
    pub source: MemorySource,
}

/// Where a reading's limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// A cgroup v2 `memory.max`.
    CgroupV2,
    /// A cgroup v1 `memory.limit_in_bytes`.
    CgroupV1,
    /// `/proc/meminfo`.
    Meminfo,
    /// The physical memory, as the operating system reports it.
    Physical,
}

impl MemorySource {
    /// Its name in `GET /api` and the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CgroupV2 => "cgroup v2 memory.max",
            Self::CgroupV1 => "cgroup v1 memory.limit_in_bytes",
            Self::Meminfo => "/proc/meminfo",
            Self::Physical => "physical memory",
        }
    }
}

/// Reads the memory the process may use; `None` when it cannot tell.
pub trait HostMemory: Send + Sync + 'static {
    /// One reading, now.
    fn read(&self) -> Option<HostReading>;
}

/// A cgroup v1 limit at or above this is no limit (the kernel writes its
/// page counter's maximum, 9223372036854771712, when none is set).
pub const V1_NO_LIMIT: u64 = 1 << 62;

/// Longest file read, in bytes: `/proc/meminfo` is about 1.5 KiB.
const MAX_FILE: u64 = 64 * 1024;

/// `/proc/meminfo`'s `MemTotal` and `MemAvailable`, in bytes; a kernel
/// before 3.14 writes no `MemAvailable`, read then as `MemFree`, `Buffers`
/// and `Cached` together. `None` without `MemTotal`.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let mut fields = [None::<u64>; 5];
    const NAMES: [&str; 5] = ["MemTotal", "MemAvailable", "MemFree", "Buffers", "Cached"];
    for line in text.lines().take(256) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let Some(slot) = NAMES.iter().position(|n| *n == name.trim()) else { continue };
        let mut words = rest.split_whitespace();
        let Some(Ok(value)) = words.next().map(str::parse::<u64>) else { continue };
        let bytes = match words.next() {
            Some("kB") => value.checked_mul(1024),
            None => Some(value),
            Some(_) => None,
        };
        if let (Some(field), Some(bytes)) = (fields.get_mut(slot), bytes) {
            field.get_or_insert(bytes);
        }
    }
    let [total, available, free, buffers, cached] = fields;
    let total = total?;
    let available = available
        .or_else(|| Some(free?.saturating_add(buffers.unwrap_or(0)).saturating_add(cached.unwrap_or(0))))
        .unwrap_or(0);
    Some((total, available.min(total)))
}

/// A cgroup v2 `memory.max`: `Some(None)` for `max` (no limit), `Some(Some(n))`
/// for a limit of `n` bytes, `None` for what is neither.
pub fn parse_v2_max(text: &str) -> Option<Option<u64>> {
    match text.trim() {
        "max" => Some(None),
        number => number.parse().ok().map(Some),
    }
}

/// A file of one number of bytes (`memory.current`, v1's
/// `memory.limit_in_bytes` and `memory.usage_in_bytes`).
pub fn parse_bytes(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `/proc/self/cgroup`'s cgroup v2 path (`0::/system.slice/oxrtc.service`)
/// and v1 memory path (`4:memory:/docker/abc`), each when present.
pub fn parse_self_cgroup(text: &str) -> (Option<String>, Option<String>) {
    let (mut v2, mut v1) = (None, None);
    for line in text.lines().take(64) {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if !path.starts_with('/') || path.contains("..") {
            continue;
        }
        if id == "0" && controllers.is_empty() {
            v2.get_or_insert_with(|| path.to_owned());
        } else if controllers.split(',').any(|c| c == "memory") {
            v1.get_or_insert_with(|| path.to_owned());
        }
    }
    (v2, v1)
}

/// The Linux reader: `/proc` and `/sys/fs/cgroup` under `root` (`/` in the
/// server, a directory of recorded files in a test).
#[derive(Debug, Clone)]
pub struct LinuxMemory {
    root: PathBuf,
}

impl Default for LinuxMemory {
    fn default() -> Self {
        Self::under("/")
    }
}

impl LinuxMemory {
    /// The files under `root`.
    pub fn under(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn file(&self, path: &str) -> Option<String> {
        let path = self.root.join(path.trim_start_matches('/'));
        let file = std::fs::File::open(path).ok()?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_FILE), &mut text).ok()?;
        Some(text)
    }

    /// The group's limit and use, by cgroup v2 then v1, the process's own
    /// group first, the namespace's root next (a container sees its own
    /// group at the root).
    fn cgroup(&self) -> Option<(u64, u64, MemorySource)> {
        let (v2, v1) = self.file("proc/self/cgroup").map(|t| parse_self_cgroup(&t)).unwrap_or_default();
        for dir in
            [v2.as_deref().map(|p| format!("sys/fs/cgroup{p}")), Some("sys/fs/cgroup".to_owned())].into_iter().flatten()
        {
            let dir = dir.trim_end_matches('/');
            if let Some(Some(limit)) = self.file(&format!("{dir}/memory.max")).and_then(|t| parse_v2_max(&t)) {
                let used = self.file(&format!("{dir}/memory.current")).and_then(|t| parse_bytes(&t)).unwrap_or(0);
                return Some((limit, used, MemorySource::CgroupV2));
            }
        }
        let v1_dirs =
            [v1.as_deref().map(|p| format!("sys/fs/cgroup/memory{p}")), Some("sys/fs/cgroup/memory".to_owned())];
        for dir in v1_dirs.into_iter().flatten() {
            let dir = dir.trim_end_matches('/');
            let Some(limit) = self.file(&format!("{dir}/memory.limit_in_bytes")).and_then(|t| parse_bytes(&t)) else {
                continue;
            };
            if limit >= V1_NO_LIMIT {
                return None;
            }
            let used = self.file(&format!("{dir}/memory.usage_in_bytes")).and_then(|t| parse_bytes(&t)).unwrap_or(0);
            return Some((limit, used, MemorySource::CgroupV1));
        }
        None
    }
}

impl HostMemory for LinuxMemory {
    fn read(&self) -> Option<HostReading> {
        let machine = self.file("proc/meminfo").and_then(|t| parse_meminfo(&t));
        match (self.cgroup(), machine) {
            (Some((limit, used, source)), machine) => {
                let mut available = limit.saturating_sub(used);
                let mut limit = limit;
                if let Some((total, free)) = machine {
                    available = available.min(free);
                    limit = limit.min(total);
                }
                Some(HostReading { limit, available, source })
            }
            (None, Some((total, available))) => {
                Some(HostReading { limit: total, available, source: MemorySource::Meminfo })
            }
            (None, None) => None,
        }
    }
}

/// A reading that never changes: a configured size, or a test's.
#[derive(Debug, Clone, Copy)]
pub struct Fixed(pub HostReading);

impl HostMemory for Fixed {
    fn read(&self) -> Option<HostReading> {
        Some(self.0)
    }
}

#[cfg(test)]
#[path = "host_memory_tests.rs"]
mod tests;
