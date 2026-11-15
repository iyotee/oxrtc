"""Preload driver: hold a stream open with no viewer, as openIVR does for every camera.

A server with a preload API (go2rtc, oxrtc: `PUT /api/preload?src=`) is asked
through it - the request answers once the source is connected, so its duration
is recorded. A server without one (MediaMTX) is held the only way it can be:
an RTSP reader that receives and discards (`ffmpeg -c copy -f null`), which
costs that server a real consumer - a difference the report states.
"""

from __future__ import annotations

import sys
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
    holder: Child | None = field(default=None, repr=False)
    result: dict = field(default_factory=dict)

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
        self.holder = Child(f"preload-{self.label}", argv, self.workdir / f"preload-{self.label}.log").start()
        self.result = {"label": self.label, "how": "rtsp-reader"}
        return self

    def stop(self) -> dict:
        if self.holder is not None:
            self.result["holder_alive_at_end"] = self.holder.alive()
            self.holder.stop()
        return self.result
