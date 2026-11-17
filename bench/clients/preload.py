"""Preload driver: hold a stream open with no viewer, as openIVR does for every camera.

A server with a preload API (go2rtc, oxrtc: `PUT /api/preload?src=`) is asked
through it - the request answers once the source is connected, so its duration
is recorded. A server without one (MediaMTX) is held the only way it can be:
an RTSP reader that receives and discards (`ffmpeg -c copy -f null`), which
costs that server a real consumer - a difference the report states.
"""

from __future__ import annotations

import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from common import Child, binary  # noqa: E402


@dataclass
class Preload:
    label: str
    stream: str
    workdir: Path
    #: Media queries tried in order until one is not answered 500 (openIVR's
    #: hold_open(): `audio=opus`, then `video=`); `""` holds everything.
    queries: list[str] = field(default_factory=lambda: [""])
    #: Dial the holder again whenever it ends before the run does (real-camera runs: a
    #: stream that was not ready when the holder first asked), as openIVR's hold_open() retries.
    keep: bool = False
    holder: Child | None = field(default=None, repr=False)
    result: dict = field(default_factory=dict)
    _stopping: threading.Event = field(default_factory=threading.Event, repr=False)
    _supervisor: threading.Thread | None = field(default=None, repr=False)
    _argv: list[str] = field(default_factory=list, repr=False)
    restarts: int = field(default=0, repr=False)

    def start(self, server) -> "Preload":
        began = time.monotonic()
        if server.supports_preload_api():
            answers = []
            for query in self.queries:
                asked = time.monotonic()
                status = server.preload(self.stream, query)
                answers.append({"query": query, "status": status, "answer_ms": (time.monotonic() - asked) * 1000})
                if status != 500:
                    break
            self.result = {"label": self.label, "how": "api", "status": answers[-1]["status"],
                           "answer_ms": (time.monotonic() - began) * 1000, "answers": answers}
            return self
        ffmpeg = binary("ffmpeg")
        if ffmpeg is None:
            raise FileNotFoundError("ffmpeg not found (set OXRTC_FFMPEG)")
        argv = [ffmpeg, "-hide_banner", "-loglevel", "error", "-nostats", "-rtsp_transport", "tcp",
                "-i", server.rtsp_url(self.stream), "-map", "0", "-c", "copy", "-f", "null", "-"]
        self._argv = argv
        self.holder = Child(f"preload-{self.label}", argv, self.workdir / f"preload-{self.label}.log").start()
        self.result = {"label": self.label, "how": "rtsp-reader"}
        if self.keep:
            self._supervisor = threading.Thread(target=self._supervise, name=f"preload-{self.label}", daemon=True)
            self._supervisor.start()
        return self

    def _supervise(self) -> None:
        while not self._stopping.is_set():
            popen = self.holder.popen if self.holder else None
            if popen is None:
                return
            try:
                popen.wait(timeout=1.0)
            except Exception:  # noqa: BLE001 - still running
                continue
            if self._stopping.wait(2.0):
                return
            self.holder.stop()
            self.restarts += 1
            self.holder = Child(f"preload-{self.label}", self._argv,
                                self.workdir / f"preload-{self.label}.log").start()

    def stop(self) -> dict:
        self._stopping.set()
        if self._supervisor is not None:
            self._supervisor.join(timeout=5)
        if self.holder is not None:
            self.result["holder_alive_at_end"] = self.holder.alive()
            self.result["restarts"] = self.restarts
            self.holder.stop()
        return self.result
