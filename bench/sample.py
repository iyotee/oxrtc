"""Process-TREE CPU and memory, sampled every second (psutil).

The tree is the root process and every descendant found at each sample: the
server under test and its FFmpeg children (go2rtc's transcodes, MediaMTX's
runOnDemand, oxrtc's sandboxed FFmpeg). CPU is cumulative user+system time,
kept per process (pid + creation time) at its last seen value, so a child that
exits between two samples still counts what it had used when last seen; a child
that lives less than one interval is missed - a known limit, documented in the
README. The machine-wide CPU is recorded beside it, so background load during
the run shows in the data.
"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field

import psutil


@dataclass
class TreeSampler:
    root_pid: int
    interval: float = 1.0
    max_samples: int = 7 * 24 * 3600  # bounded: a week at 1 Hz
    samples: list[dict] = field(default_factory=list)
    _stop: threading.Event = field(default_factory=threading.Event, repr=False)
    _thread: threading.Thread | None = field(default=None, repr=False)
    _cpu: dict[tuple[int, float], float] = field(default_factory=dict, repr=False)
    _procs: dict[tuple[int, float], psutil.Process] = field(default_factory=dict, repr=False)
    #: Creation time of the root when first seen: a reused pid is not our server.
    _root_created: float | None = field(default=None, repr=False)

    def start(self) -> "TreeSampler":
        psutil.cpu_percent(None)  # prime the machine-wide counter
        self._thread = threading.Thread(target=self._loop, name="tree-sampler", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> list[dict]:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=5)
        return self.samples

    def _tree(self) -> list[psutil.Process]:
        try:
            root = psutil.Process(self.root_pid)
            created = root.create_time()
        except psutil.Error:
            return []
        if self._root_created is None:
            self._root_created = created
        elif created != self._root_created:
            return []  # the server exited and its pid now belongs to another process
        try:
            return [root, *root.children(recursive=True)]
        except psutil.NoSuchProcess:
            return [root]

    def sample_once(self, t0: float, previous: dict | None) -> dict:
        now = time.monotonic()
        rss = private = 0
        threads = handles = 0
        live = 0
        for proc in self._tree():
            try:
                key = (proc.pid, proc.create_time())
                proc = self._procs.setdefault(key, proc)
                with proc.oneshot():
                    times = proc.cpu_times()
                    mem = proc.memory_info()
                    threads += proc.num_threads()
                    handles += proc.num_handles() if hasattr(proc, "num_handles") else proc.num_fds()
            except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
                continue
            self._cpu[key] = times.user + times.system
            rss += mem.rss
            private += getattr(mem, "private", 0) or getattr(mem, "uss", 0)
            live += 1
        cpu_total = sum(self._cpu.values())
        sample = {
            "t": round(now - t0, 3),
            "cpu_s": round(cpu_total, 4),
            "rss_mb": round(rss / 2**20, 2),
            "private_mb": round(private / 2**20, 2) if private else None,
            "procs": live,
            "threads": threads,
            "handles": handles,
            "machine_cpu_pct": psutil.cpu_percent(None),
        }
        if previous is not None:
            dt = sample["t"] - previous["t"]
            if dt > 0:
                core_pct = 100.0 * (cpu_total - previous["cpu_s"]) / dt
                sample["cpu_pct_core"] = round(core_pct, 2)
                sample["cpu_pct_machine"] = round(core_pct / (psutil.cpu_count() or 1), 3)
        return sample

    def _loop(self) -> None:
        t0 = time.monotonic()
        previous = None
        while not self._stop.is_set() and len(self.samples) < self.max_samples:
            previous = self.sample_once(t0, previous)
            self.samples.append(previous)
            self._stop.wait(self.interval)


def background_load(seconds: float = 3.0, top: int = 5) -> dict:
    """Machine load just before a run: overall CPU, memory, and the busiest processes."""
    procs = []
    for proc in psutil.process_iter(["name"]):
        try:
            proc.cpu_percent(None)
            procs.append(proc)
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            continue
    overall = psutil.cpu_percent(interval=seconds)
    busy = []
    for proc in procs:
        try:
            busy.append((proc.cpu_percent(None), proc.info["name"]))
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            continue
    busy.sort(reverse=True)
    cores = psutil.cpu_count() or 1
    return {
        "window_s": seconds,
        "machine_cpu_pct": overall,
        "memory_available_mb": round(psutil.virtual_memory().available / 2**20),
        "top": [{"name": name, "cpu_pct_machine": round(pct / cores, 2)} for pct, name in busy[:top]],
    }


def summarize(samples: list[dict], skip_s: float = 2.0) -> dict:
    """Per-run figures from the samples: means over the steady part (after `skip_s`)."""
    steady = [s for s in samples if s.get("t", 0) >= skip_s and "cpu_pct_core" in s]
    if not steady:
        return {}

    def mean(key: str) -> float | None:
        values = [s[key] for s in steady if s.get(key) is not None]
        return sum(values) / len(values) if values else None

    first, last = steady[0], steady[-1]
    span = last["t"] - first["t"]
    return {
        "cpu_pct_core_mean": mean("cpu_pct_core"),
        "cpu_pct_machine_mean": mean("cpu_pct_machine"),
        "cpu_s_per_s": (last["cpu_s"] - first["cpu_s"]) / span if span > 0 else None,
        "rss_mb_mean": mean("rss_mb"),
        "rss_mb_max": max(s["rss_mb"] for s in steady),
        "private_mb_mean": mean("private_mb"),
        "machine_cpu_pct_mean": mean("machine_cpu_pct"),
        "handles_max": max(s["handles"] for s in steady),
        "threads_max": max(s["threads"] for s in steady),
    }
