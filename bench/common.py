"""Shared plumbing of the bench: binaries, ports, processes, machine facts.

Every process the bench starts is a `Child`: started from an argument vector
(never a shell), with its own stdout/stderr log file, and stopped through its
own handle - the process and its descendants, found by psutil from OUR pid,
never by name. The owner's NVR and its FFmpeg processes run on the same
machine; a sweep by name would cut them.
"""

from __future__ import annotations

import os
import platform
import random
import socket
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

import psutil

BENCH = Path(__file__).resolve().parent
REPO = BENCH.parent
RESULTS = BENCH / "results"

DEFAULT_BINARIES = {
    "ffmpeg": ("OXRTC_FFMPEG", "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffmpeg.exe"),
    "ffprobe": ("OXRTC_FFPROBE", "C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/ffprobe.exe"),
    "go2rtc": ("OXRTC_GO2RTC", str(REPO.parent / "bench-bin" / "go2rtc-1.9.15" / ("go2rtc.exe" if os.name == "nt" else "go2rtc"))),
    "oxrtc": ("OXRTC_BIN", str(REPO / "target" / "release" / ("oxrtc.exe" if os.name == "nt" else "oxrtc"))),
    "mediamtx": ("OXRTC_MEDIAMTX", "mediamtx.exe" if os.name == "nt" else "mediamtx"),
    "rust2rtc_gpl": ("OXRTC_RUST2RTC_GPL", "rust2rtc.exe" if os.name == "nt" else "rust2rtc"),
    "node": ("OXRTC_NODE", "node"),
}

#: Ports the owner's NVR and benches use on this machine: never bound by the bench.
FORBIDDEN_PORTS = frozenset(
    {1984, 8554, 8555, 7998, 7999, 8000, 8443, 11984, 18554, 18555, 18998, 21984, 28554}
)


def binary(name: str) -> str | None:
    """The path of a binary, from its environment variable or its default; None if absent."""
    env, default = DEFAULT_BINARIES[name]
    candidate = os.environ.get(env) or default
    path = Path(candidate)
    if path.is_file():
        return str(path)
    found = _which(candidate)
    return found


def _which(name: str) -> str | None:
    from shutil import which

    return which(name)


def free_port() -> int:
    """A free TCP port on 127.0.0.1, chosen by the OS, never one of the owner's."""
    for _ in range(50):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        if port not in FORBIDDEN_PORTS:
            return port
    raise RuntimeError("no free port found")


def free_udp_port() -> int:
    """A port free for both TCP and UDP (WebRTC listeners use both).

    Candidates are drawn at random, not from the OS's ephemeral counter: on
    Windows, Hyper-V/WSL reserve whole TCP ranges (WinError 10013), and the
    UDP counter's consecutive ports can sit inside one for every attempt.
    """
    last: OSError | None = None
    for _ in range(200):
        port = random.randint(20000, 60999)  # noqa: S311 - not a secret
        if port in FORBIDDEN_PORTS:
            continue
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as tcp, \
                    socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                tcp.bind(("127.0.0.1", port))
                udp.bind(("127.0.0.1", port))
        except OSError as err:
            last = err
            continue
        return port
    raise RuntimeError(f"no free TCP+UDP port found ({last})")


class _Job:
    """A Windows job object with KILL_ON_JOB_CLOSE holding one child and its descendants.

    Why: stopping a tree by walking psutil's children misses a grandchild whose
    parent already exited (go2rtc's FFmpeg after go2rtc died), and nothing at
    all is stopped when the runner itself dies abruptly (killed, console
    closed) - its go2rtc, FFmpeg, node and Edge then outlive it. A job closes
    with the last handle to it, which the OS closes when the runner dies, and
    kills every process in it. Descendants started after the assignment are in
    the job too. Windows only; elsewhere `create` returns None and the psutil
    walk is the only mechanism (documented in the README).
    """

    _KILL_ON_JOB_CLOSE = 0x2000
    _EXTENDED_LIMIT_INFORMATION = 9
    _PROCESS_SET_QUOTA = 0x0100
    _PROCESS_TERMINATE = 0x0001
    _kernel32 = None

    def __init__(self, handle: int):
        self.handle = handle

    @classmethod
    def _api(cls):
        if cls._kernel32 is None:
            import ctypes
            from ctypes import wintypes

            k = ctypes.WinDLL("kernel32", use_last_error=True)
            k.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
            k.CreateJobObjectW.restype = wintypes.HANDLE
            k.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
            k.SetInformationJobObject.restype = wintypes.BOOL
            k.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
            k.AssignProcessToJobObject.restype = wintypes.BOOL
            k.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
            k.TerminateJobObject.restype = wintypes.BOOL
            k.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            k.OpenProcess.restype = wintypes.HANDLE
            k.CloseHandle.argtypes = [wintypes.HANDLE]
            k.CloseHandle.restype = wintypes.BOOL
            cls._kernel32 = k
        return cls._kernel32

    @classmethod
    def create(cls, pid: int) -> "_Job | None":
        """A job holding process `pid` (just started by us), or None (not Windows, or refused)."""
        if os.name != "nt":
            return None
        import ctypes

        class Basic(ctypes.Structure):
            _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                        ("LimitFlags", ctypes.c_uint32), ("MinimumWorkingSetSize", ctypes.c_size_t),
                        ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", ctypes.c_uint32),
                        ("Affinity", ctypes.c_size_t), ("PriorityClass", ctypes.c_uint32),
                        ("SchedulingClass", ctypes.c_uint32)]

        class Extended(ctypes.Structure):
            _fields_ = [("Basic", Basic), ("IoInfo", ctypes.c_uint64 * 6),
                        ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                        ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]

        k = cls._api()
        job = k.CreateJobObjectW(None, None)
        if not job:
            return None
        info = Extended()
        info.Basic.LimitFlags = cls._KILL_ON_JOB_CLOSE
        proc = None
        try:
            if not k.SetInformationJobObject(job, cls._EXTENDED_LIMIT_INFORMATION, ctypes.byref(info),
                                             ctypes.sizeof(info)):
                raise OSError(ctypes.get_last_error(), "SetInformationJobObject")
            proc = k.OpenProcess(cls._PROCESS_SET_QUOTA | cls._PROCESS_TERMINATE, False, pid)
            if not proc or not k.AssignProcessToJobObject(job, proc):
                raise OSError(ctypes.get_last_error(), "AssignProcessToJobObject")
        except OSError:
            k.CloseHandle(job)
            return None
        finally:
            if proc:
                k.CloseHandle(proc)
        return cls(job)

    def close(self) -> None:
        """Kill everything still in the job, and release it."""
        if self.handle:
            k = self._api()
            k.TerminateJobObject(self.handle, 1)
            k.CloseHandle(self.handle)
            self.handle = 0


def contain(popen: subprocess.Popen) -> _Job | None:
    """Tie `popen` (and what it starts later) to this process's life; see _Job."""
    return _Job.create(popen.pid)


@dataclass
class Child:
    """A process the bench started, stopped only through this handle."""

    name: str
    argv: list[str]
    log_path: Path
    cwd: Path | None = None
    popen: subprocess.Popen | None = field(default=None, repr=False)
    #: True when the OS kills the child's whole tree if this process dies (Windows job).
    contained: bool = False

    def start(self) -> "Child":
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        log = open(self.log_path, "wb")  # noqa: SIM115 - closed in stop()
        flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
        try:
            self.popen = subprocess.Popen(  # noqa: S603 - argument vector, no shell
                self.argv,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                cwd=self.cwd,
                creationflags=flags,
            )
        except BaseException:
            log.close()
            raise
        self._log = log
        self._stopped = False
        self._job = contain(self.popen)
        self.contained = self._job is not None
        try:
            self._created: float | None = psutil.Process(self.popen.pid).create_time()
        except psutil.Error:
            self._created = None
        return self

    @property
    def pid(self) -> int:
        if self.popen is None:
            raise RuntimeError(f"{self.name} not started")
        return self.popen.pid

    def alive(self) -> bool:
        return self.popen is not None and self.popen.poll() is None

    def stop(self, grace: float = 5.0) -> None:
        """Stop the process and every descendant of it; idempotent.

        On Windows the job holds the whole tree, orphans included: closing it
        kills them all, no pid is looked up. Elsewhere (or if the job was
        refused) the tree is walked from OUR pid, and only while our process
        is still running and still the one we started (same creation time):
        once it has exited and been reaped its pid may belong to someone else,
        and terminating "its children" would cut a stranger's tree.
        """
        if self.popen is None or getattr(self, "_stopped", True):
            return
        self._stopped = True
        procs: list[psutil.Process] = []
        if self._job is not None:
            self._job.close()
        elif self.popen.poll() is None:
            try:
                root = psutil.Process(self.popen.pid)
                if self._created is None or root.create_time() == self._created:
                    procs = [root, *root.children(recursive=True)]
            except psutil.Error:
                pass
        for proc in procs:
            try:
                proc.terminate()
            except psutil.Error:
                pass
        _, alive = psutil.wait_procs(procs, timeout=grace)
        for proc in alive:
            try:
                proc.kill()
            except psutil.Error:
                pass
        psutil.wait_procs(alive, timeout=grace)
        try:
            self.popen.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            pass
        self._log.close()

    def tail(self, lines: int = 20) -> str:
        try:
            text = self.log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""
        return "\n".join(text.splitlines()[-lines:])


def wait_until(predicate, timeout: float, interval: float = 0.2) -> bool:
    """Poll *predicate* until it is true or *timeout* seconds have passed."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if predicate():
                return True
        except Exception:  # noqa: BLE001 - a probe that throws is "not yet"
            pass
        time.sleep(interval)
    return False


def binary_version(path: str | None, args: list[str]) -> str | None:
    """First line of `<binary> <args>`, for the run's record."""
    if not path:
        return None
    try:
        out = subprocess.run(  # noqa: S603
            [path, *args], capture_output=True, text=True, timeout=10, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    text = (out.stdout or out.stderr).strip()
    return text.splitlines()[0][:200] if text else None


def machine_facts() -> dict:
    """What the machine is, recorded with every run (no host name, no user name)."""
    freq = psutil.cpu_freq()
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "os_version": platform.version(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "cpu_logical": psutil.cpu_count(logical=True),
        "cpu_physical": psutil.cpu_count(logical=False),
        "cpu_max_mhz": freq.max if freq else None,
        "memory_total_mb": round(psutil.virtual_memory().total / 2**20),
        "python": sys.version.split()[0],
    }
