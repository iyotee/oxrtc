# Host memory fixtures

What `oxrtc_core::host_memory::LinuxMemory` reads, under a root per case (`proc/meminfo`,
`proc/self/cgroup`, `sys/fs/cgroup/...`), for `crates/core/src/host_memory_tests.rs`.

- `msys/proc/meminfo`: recorded on the owner's PC (Git for Windows' MSYS2, which writes a
  `/proc/meminfo` with no `MemAvailable`, so the older kernels' rule is read: `MemFree`,
  `Buffers`, `Cached`), 2026-11-23.
- The others are written in the kernel's own formats (Documentation/filesystems/proc.rst,
  Documentation/admin-guide/cgroup-v2.rst and cgroup-v1/memory.rst) with the numbers of the
  cases they stand for: a Raspberry Pi 5 of 8 GiB running the server in Docker with `--memory
  2g` (cgroup v2, the container's own group at the root of its namespace), a systemd service with
  `MemoryMax=1G` (cgroup v2, the group named in `/proc/self/cgroup`), a group with no limit
  (`max`), and cgroup v1 with a limit and without one (the page counter's maximum).
