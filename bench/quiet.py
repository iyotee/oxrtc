"""A quiet machine for every measured run, when other work shares the machine.

A compiler or a test suite running beside a measured run makes its CPU, memory and
latency figures worthless. When a hook command is configured, every run is wrapped:

    <hook> acquire <who>    before the run; it returns once the machine is quiet
    <hook> release <who>    after the run, whatever its outcome

The hook is ``OXRTC_BENCH_HOOK``, else the first line of ``bench/.bench-hook`` that is
neither empty nor a ``#`` comment (a local, git-ignored file), else nothing: with no
hook every run starts at once, as before. A hook that fails or cannot be started is
printed and the run goes on, unlocked.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import time
from collections.abc import Callable, Iterator, Mapping
from contextlib import contextmanager
from pathlib import Path

HOOK_FILE = Path(__file__).resolve().parent / ".bench-hook"
#: The longest an acquire may take: the hook's own wait (20 minutes) and a margin.
ACQUIRE_TIMEOUT_S = 30 * 60
RELEASE_TIMEOUT_S = 60


def hook_command(env: Mapping[str, str] | None = None, hook_file: Path = HOOK_FILE) -> list[str]:
    """The hook as an argument list, or [] when none is configured."""
    env = os.environ if env is None else env
    text = env.get("OXRTC_BENCH_HOOK", "").strip()
    if not text and hook_file.is_file():
        lines = (line.strip() for line in hook_file.read_text(encoding="utf-8").splitlines())
        text = next((line for line in lines if line and not line.startswith("#")), "")
    return shlex.split(text) if text else []


def _call(cmd: list[str], verb: str, who: str, timeout: float) -> str | None:
    """One hook call: None when it succeeded, else what went wrong."""
    try:
        done = subprocess.run([*cmd, verb, who], capture_output=True, text=True, timeout=timeout, check=False)
    except (OSError, subprocess.SubprocessError) as err:
        return f"{verb} could not run: {err}"
    if done.returncode != 0:
        said = (done.stdout + done.stderr).strip()[-200:]
        return f"{verb} exited {done.returncode}: {said}"
    return None


@contextmanager
def quiet_machine(who: str, cmd: list[str] | None = None, out: Callable[..., None] = print) -> Iterator[float]:
    """Holds the hook's lock around the body; yields how long the acquire waited, in seconds."""
    cmd = hook_command() if cmd is None else cmd
    if not cmd:
        yield 0.0
        return
    start = time.monotonic()
    problem = _call(cmd, "acquire", who, ACQUIRE_TIMEOUT_S)
    waited = time.monotonic() - start
    out(f"  quiet machine: {problem or f'held after {waited:.0f} s'}", flush=True)
    try:
        yield waited
    finally:
        problem = _call(cmd, "release", who, RELEASE_TIMEOUT_S)
        if problem:
            out(f"  quiet machine: {problem}", flush=True)
