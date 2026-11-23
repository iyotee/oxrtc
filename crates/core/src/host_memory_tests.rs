use std::path::PathBuf;

use proptest::prelude::*;

use super::*;

fn fixture(case: &str) -> LinuxMemory {
    LinuxMemory::under(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_memory").join(case))
}

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn meminfo_is_read_in_bytes_and_an_old_kernel_s_available_is_summed() {
    let pi =
        "MemTotal:        8245124 kB\nMemFree:         5120336 kB\nMemAvailable:    6912440 kB\nBuffers: 64320 kB\n";
    assert_eq!(parse_meminfo(pi), Some((8_245_124 * 1024, 6_912_440 * 1024)));
    let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 20 kB\nCached: 300 kB\n";
    assert_eq!(parse_meminfo(old), Some((1_024_000, 420 * 1024)));
    // No MemTotal: nothing; available never above the total.
    assert_eq!(parse_meminfo("MemFree: 5 kB\n"), None);
    assert_eq!(parse_meminfo("MemTotal: 10 kB\nMemAvailable: 99 kB\n"), Some((10_240, 10_240)));
    assert_eq!(parse_meminfo("MemTotal: 12 MB\n"), None, "a unit the kernel never writes");
}

#[test]
fn the_cgroup_files_are_read() {
    assert_eq!(parse_v2_max("max\n"), Some(None));
    assert_eq!(parse_v2_max("2147483648\n"), Some(Some(2 * GIB)));
    assert_eq!(parse_v2_max("lots"), None);
    assert_eq!(parse_bytes(" 412876800\n"), Some(412_876_800));
    let both = "12:pids:/docker/3f2a\n11:memory:/docker/3f2a\n0::/system.slice/oxrtc.service\n";
    assert_eq!(
        parse_self_cgroup(both),
        (Some("/system.slice/oxrtc.service".to_owned()), Some("/docker/3f2a".to_owned()))
    );
    // A path that climbs out of the hierarchy is not followed.
    assert_eq!(parse_self_cgroup("0::/../../etc\n"), (None, None));
}

#[test]
fn a_docker_container_on_a_pi_is_read_from_its_own_group() {
    let read = fixture("docker_v2").read().unwrap();
    assert_eq!(read.source, MemorySource::CgroupV2);
    assert_eq!(read.limit, 2 * GIB, "the container's --memory 2g, under the machine's 8 GiB");
    assert_eq!(read.available, 2 * GIB - 412_876_800, "what the group may still take");
}

#[test]
fn a_service_is_read_from_the_group_proc_self_cgroup_names() {
    let read = fixture("service_v2").read().unwrap();
    assert_eq!((read.source, read.limit, read.available), (MemorySource::CgroupV2, GIB, GIB - 104_857_600));
}

#[test]
fn a_group_without_a_limit_is_the_machine_s_memory() {
    let read = fixture("unlimited_v2").read().unwrap();
    assert_eq!((read.source, read.limit, read.available), (MemorySource::Meminfo, 8_245_124 * 1024, 6_912_440 * 1024));
    let read = fixture("v1_unlimited").read().unwrap();
    assert_eq!(read.source, MemorySource::Meminfo, "v1's page counter maximum is no limit");
}

#[test]
fn cgroup_v1_is_read_when_there_is_no_v2_limit() {
    let read = fixture("docker_v1").read().unwrap();
    assert_eq!((read.source, read.limit, read.available), (MemorySource::CgroupV1, 512 << 20, 256 << 20));
}

#[test]
fn a_machine_with_no_kernel_available_figure_is_read_from_msys_s_meminfo() {
    // Recorded on the owner's PC: Git for Windows' /proc/meminfo has no MemAvailable.
    let read = fixture("msys").read().unwrap();
    assert_eq!(read.source, MemorySource::Meminfo);
    assert!(read.limit > 64 * GIB && read.available <= read.limit && read.available > 0, "{read:?}");
    assert!(fixture("nowhere").read().is_none(), "no file: no reading");
}

/// The real host this test runs on, when it is Linux (the CI's runners and
/// containers): a reading, its numbers sane.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_host_this_runs_on_is_read() {
    let read = LinuxMemory::default().read().expect("a Linux host has /proc/meminfo");
    assert!(read.limit > 0 && read.available <= read.limit, "{read:?}");
}

proptest! {
    /// The stable twin of the readers' input: any text a file could hold is
    /// read or refused, never a panic, and what is read is consistent.
    #[test]
    fn any_file_text_is_read_or_refused(text in "[ -~\n]{0,400}", bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        for input in [text.clone(), String::from_utf8_lossy(&bytes).into_owned()] {
            if let Some((total, available)) = parse_meminfo(&input) {
                prop_assert!(available <= total);
            }
            let _ = parse_v2_max(&input);
            let _ = parse_bytes(&input);
            let (v2, v1) = parse_self_cgroup(&input);
            for path in [v2, v1].into_iter().flatten() {
                prop_assert!(path.starts_with('/') && !path.contains(".."));
            }
        }
    }

    /// A meminfo written from any two numbers is read back as them.
    #[test]
    fn a_written_meminfo_is_read_as_written(total in 0u64..1 << 40, available in 0u64..1 << 40) {
        let text = format!("MemTotal: {total} kB\nMemFree: 1 kB\nMemAvailable: {available} kB\n");
        prop_assert_eq!(parse_meminfo(&text), Some((total * 1024, (available * 1024).min(total * 1024))));
    }
}
