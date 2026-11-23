//! The output memory budget of the server (D45): sized from `api.output_memory`
//! over what the host gives the process, set on the hub, read again every
//! few seconds until the server stops.

use std::sync::Arc;

use oxrtc_api::config::Config;
use oxrtc_core::CancellationToken;
use oxrtc_core::host_memory::HostMemory;
use oxrtc_core::memory::{BudgetSize, MemoryBudget, REFRESH_EVERY, source_name};

/// The host's memory as this system reads it: the control group and
/// `/proc/meminfo` on Linux.
#[cfg(target_os = "linux")]
fn host() -> Box<dyn HostMemory> {
    Box::new(oxrtc_core::host_memory::LinuxMemory::default())
}

/// The host's memory as this system reads it: the physical memory and what
/// of it is available (`sysinfo`, already the tests' process lister: no
/// code of this project's touches the operating system's own calls).
#[cfg(not(target_os = "linux"))]
fn host() -> Box<dyn HostMemory> {
    Box::new(Physical)
}

#[cfg(not(target_os = "linux"))]
struct Physical;

#[cfg(not(target_os = "linux"))]
impl HostMemory for Physical {
    fn read(&self) -> Option<oxrtc_core::host_memory::HostReading> {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let limit = system.total_memory();
        (limit > 0).then(|| oxrtc_core::host_memory::HostReading {
            limit,
            available: system.available_memory().min(limit),
            source: oxrtc_core::host_memory::MemorySource::Physical,
        })
    }
}

/// The budget `config` asks for, read once, and said in the log.
pub(crate) fn budget(config: &Config) -> Arc<MemoryBudget> {
    let size = config.api.output_memory.map_or_else(BudgetSize::default, |m| m.size());
    let budget = MemoryBudget::new(size, host());
    let snapshot = budget.snapshot();
    tracing::info!(
        budget = snapshot.budget,
        limit = snapshot.reading.map(|r| r.limit),
        available = snapshot.reading.map(|r| r.available),
        source = source_name(snapshot.reading),
        "api: output memory budget (api.output_memory)"
    );
    budget
}

/// Reads the host again every [`REFRESH_EVERY`] until `stop`.
pub(crate) async fn refresh(budget: Arc<MemoryBudget>, stop: CancellationToken) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(REFRESH_EVERY) => {}
        }
        budget.refresh();
    }
}
