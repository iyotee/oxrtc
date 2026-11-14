"""Every process the bench starts dies with it: stopped by handle, or with the runner.

The owner's NVR and its FFmpeg processes run on this machine, so the bench may
only ever stop processes it started itself. These tests start their own
sleepers (Python, and FFmpeg when it is installed) and check three claims:

- killing the runner abruptly (TerminateProcess, no `finally`) kills its
  children and their descendants (Windows job object);
- `Child.stop()` kills a grandchild whose own parent already exited (an
  orphan no psutil walk from our pid can find);
- `Child.stop()` never terminates a process that is not the one it started
  (pid reuse), and is idempotent.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import psutil

BENCH = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH))

from common import Child, binary  # noqa: E402

PY_SLEEPER = [sys.executable, "-c", "import time; time.sleep(120)"]


def ffmpeg_sleeper(ffmpeg: str) -> list[str]:
    """An FFmpeg that runs until killed, at almost no cost (1 fps of 16x16, real time)."""
    return [ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-re", "-f", "lavfi",
            "-i", "nullsrc=s=16x16:r=1", "-f", "null", "-"]


def gone(pid: int, created: float, timeout: float = 10.0) -> bool:
    """True once process `pid` with creation time `created` no longer exists."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            proc = psutil.Process(pid)
            if proc.create_time() != created or proc.status() == psutil.STATUS_ZOMBIE:
                return True
        except psutil.NoSuchProcess:
            return True
        time.sleep(0.1)
    return False


def kill_if_ours(pid: int, created: float) -> None:
    """Clean up after a failed assertion: only the exact process this test started."""
    try:
        proc = psutil.Process(pid)
        if proc.create_time() == created:
            proc.kill()
    except psutil.Error:
        pass


@unittest.skipUnless(os.name == "nt", "job objects are Windows-only (README: Known limits)")
class DiesWithTheRunner(unittest.TestCase):
    def _owner_killed(self, argv: list[str]) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            script = (
                "import sys, time\n"
                f"sys.path.insert(0, {str(BENCH)!r})\n"
                "from pathlib import Path\n"
                "from common import Child\n"
                f"c = Child('t', {argv!r}, Path({str(Path(tmp) / 'child.log')!r})).start()\n"
                "print(c.pid, c.contained, flush=True)\n"
                "time.sleep(120)\n"
            )
            owner = subprocess.Popen([sys.executable, "-c", script], stdout=subprocess.PIPE, text=True)  # noqa: S603
            try:
                pid_text, contained = (owner.stdout.readline() if owner.stdout else "").split()
                pid = int(pid_text)
                created = psutil.Process(pid).create_time()
                self.assertEqual(contained, "True", "the child was not put in a job")
                owner.kill()  # abrupt: no finally, no stop()
                owner.wait(timeout=10)
                try:
                    self.assertTrue(gone(pid, created), f"{argv[0]} outlived its killed owner")
                finally:
                    kill_if_ours(pid, created)
            finally:
                if owner.poll() is None:
                    owner.kill()
                    owner.wait(timeout=10)
                if owner.stdout:
                    owner.stdout.close()

    def test_python_child_dies_with_its_killed_owner(self):
        self._owner_killed(PY_SLEEPER)

    def test_ffmpeg_child_dies_with_its_killed_owner(self):
        ffmpeg = binary("ffmpeg")
        if ffmpeg is None:
            self.skipTest("ffmpeg not found (OXRTC_FFMPEG)")
        self._owner_killed(ffmpeg_sleeper(ffmpeg))

    def test_stop_kills_an_orphaned_grandchild(self):
        ffmpeg = binary("ffmpeg")
        grand = ffmpeg_sleeper(ffmpeg) if ffmpeg else PY_SLEEPER
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "grandchild.json"
            # The child starts the grandchild, writes its pid, and exits at once.
            script = (
                "import json, subprocess, sys\n"
                f"p = subprocess.Popen({grand!r}, stdin=subprocess.DEVNULL)\n"
                f"open({str(out)!r}, 'w').write(json.dumps(p.pid))\n"
            )
            child = Child("parent", [sys.executable, "-c", script], Path(tmp) / "parent.log").start()
            child.popen.wait(timeout=30)  # type: ignore[union-attr]
            pid = json.loads(out.read_text())
            created = psutil.Process(pid).create_time()
            try:
                self.assertFalse(child.alive())
                child.stop()
                self.assertTrue(gone(pid, created), "the orphaned grandchild survived stop()")
            finally:
                kill_if_ours(pid, created)


class StopIsSafe(unittest.TestCase):
    def test_stop_twice_is_a_no_op(self):
        with tempfile.TemporaryDirectory() as tmp:
            child = Child("short", [sys.executable, "-c", "pass"], Path(tmp) / "c.log").start()
            child.popen.wait(timeout=30)  # type: ignore[union-attr]
            child.stop()
            child.stop()
            self.assertFalse(child.alive())

    def test_stop_never_terminates_a_process_it_did_not_start(self):
        """With no job, a pid whose creation time differs is someone else's: left alone."""
        with tempfile.TemporaryDirectory() as tmp:
            child = Child("sleeper", PY_SLEEPER, Path(tmp) / "c.log").start()
            try:
                if child._job is not None:  # take the psutil path that a reused pid would reach
                    job, child._job = child._job, None
                else:
                    job = None
                child._created = 1.0  # as if the pid now belonged to a process started in 1970
                child.stop(grace=0.5)
                self.assertTrue(child.alive(), "stop() terminated a process with another creation time")
            finally:
                child.popen.kill()  # type: ignore[union-attr]
                child.popen.wait(timeout=10)  # type: ignore[union-attr]
                if job is not None:
                    job.close()

    def test_stop_kills_a_running_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            child = Child("sleeper", PY_SLEEPER, Path(tmp) / "c.log").start()
            created = psutil.Process(child.pid).create_time()
            child.stop()
            self.assertTrue(gone(child.pid, created))


if __name__ == "__main__":
    unittest.main()
