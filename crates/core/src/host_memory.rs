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

/// The value of `key` in a `memory.stat` (`inactive_file 3087007744`, one
/// key and its number of bytes a line), `None` when it is not there.
pub fn parse_memory_stat(text: &str, key: &str) -> Option<u64> {
    text.lines().take(256).find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == key).then(|| words.next()?.parse().ok()).flatten()
    })
}

/// What a group uses that it cannot give back on demand: its use less its
/// inactive file pages (the page cache of what was written or read and not
/// touched since, which the kernel reclaims before it refuses the group
/// memory), as `docker stats` and the kubelet's working set count it. A
/// group that records video to disk has its use near its limit for ever,
/// most of it cache: read as used, that would leave the budget nothing.
fn working_set(used: u64, stat: Option<&str>, key: &str) -> u64 {
    used.saturating_sub(stat.and_then(|t| parse_memory_stat(t, key)).unwrap_or(0))
}

/// Deepest control group path followed up to its root.
const MAX_GROUP_DEPTH: usize = 32;

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

    /// The group's limit and what it may still take, by cgroup v2 then v1.
    /// On v2 the process's own group and each of its parents up to the
    /// namespace's root are read (a container sees its own group at the
    /// root; a systemd slice or a pod sets its limit on a parent), the
    /// tightest of them kept: the smallest limit, and the least left under
    /// any of them. What a group uses is its working set ([`working_set`]):
    /// its use less its inactive page cache.
    fn cgroup(&self) -> Option<(u64, u64, MemorySource)> {
        let (v2, v1) = self.file("proc/self/cgroup").map(|t| parse_self_cgroup(&t)).unwrap_or_default();
        let mut tightest: Option<(u64, u64)> = None;
        for dir in v2_groups(v2.as_deref()) {
            let Some(Some(limit)) = self.file(&format!("{dir}/memory.max")).and_then(|t| parse_v2_max(&t)) else {
                continue;
            };
            let used = self.file(&format!("{dir}/memory.current")).and_then(|t| parse_bytes(&t)).unwrap_or(0);
            let used = working_set(used, self.file(&format!("{dir}/memory.stat")).as_deref(), "inactive_file");
            let left = limit.saturating_sub(used);
            tightest = Some(tightest.map_or((limit, left), |(l, a)| (l.min(limit), a.min(left))));
        }
        if let Some((limit, available)) = tightest {
            return Some((limit, available, MemorySource::CgroupV2));
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
            let used = working_set(used, self.file(&format!("{dir}/memory.stat")).as_deref(), "total_inactive_file");
            return Some((limit, limit.saturating_sub(used), MemorySource::CgroupV1));
        }
        None
    }
}

/// The cgroup v2 directories of the group at `path` and of each of its
/// parents, the root last (`sys/fs/cgroup` alone without a path), at most
/// [`MAX_GROUP_DEPTH`] of them.
fn v2_groups(path: Option<&str>) -> Vec<String> {
    let mut groups = Vec::new();
    let mut path = path.unwrap_or("/").trim_end_matches('/');
    while !path.is_empty() && groups.len() < MAX_GROUP_DEPTH {
        groups.push(format!("sys/fs/cgroup{path}"));
        path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
    groups.push("sys/fs/cgroup".to_owned());
    groups
}

impl HostMemory for LinuxMemory {
    fn read(&self) -> Option<HostReading> {
        let machine = self.file("proc/meminfo").and_then(|t| parse_meminfo(&t));
        match (self.cgroup(), machine) {
            (Some((limit, available, source)), machine) => {
                let (mut limit, mut available) = (limit, available);
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
