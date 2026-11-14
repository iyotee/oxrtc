"""RTSP readers: ffprobe processes that count what a consumer actually receives.

Each reader is one ffprobe over TCP interleaved (as every openIVR reader is),
reading for the run's duration and counting the video stream's packets
(`-count_packets`, no decoding: the reader should cost the machine as little
as possible, so it does not compete with the server under test). `frames=True`
decodes and counts frames instead, for a scenario that needs it.
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from common import Child, binary  # noqa: E402


@dataclass
class RtspReader:
    label: str
    url: str
    seconds: float
    workdir: Path
    frames: bool = False
    child: Child | None = field(default=None, repr=False)

    def argv(self, ffprobe: str) -> list[str]:
        count = "-count_frames" if self.frames else "-count_packets"
        entry = "stream=codec_name,width,height,nb_read_frames" if self.frames else \
            "stream=codec_name,width,height,nb_read_packets"
        return [
            ffprobe, "-v", "error", "-rtsp_transport", "tcp", "-timeout", "10000000",
            "-select_streams", "v:0", "-read_intervals", f"%+{self.seconds:g}",
            count, "-show_entries", entry, "-of", "json", self.url,
        ]

    @property
    def out_path(self) -> Path:
        return self.workdir / f"reader-{self.label}.json"

    def start(self) -> "RtspReader":
        ffprobe = binary("ffprobe")
        if ffprobe is None:
            raise FileNotFoundError("ffprobe not found (set OXRTC_FFPROBE)")
        self.child = Child(f"reader-{self.label}", self.argv(ffprobe), self.out_path).start()
        return self

    def result(self) -> dict:
        """Stop if still running, parse ffprobe's JSON (stdout and stderr share the file)."""
        if self.child is None:
            return {"label": self.label, "error": "not started"}
        if self.child.popen is not None:
            try:
                self.child.popen.wait(timeout=self.seconds + 15)
            except Exception:  # noqa: BLE001 - a reader that hangs is stopped below
                pass
        self.child.stop()
        text = self.out_path.read_text(encoding="utf-8", errors="replace")
        start = text.find("{")
        try:
            data = json.loads(text[start:]) if start >= 0 else {}
        except ValueError:
            return {"label": self.label, "error": text[-300:]}
        streams = data.get("streams") or [{}]
        stream = streams[0]
        count = stream.get("nb_read_frames" if self.frames else "nb_read_packets")
        try:
            count = int(count)
        except (TypeError, ValueError):
            count = None
        return {
            "label": self.label,
            "codec": stream.get("codec_name"),
            "width": stream.get("width"),
            "height": stream.get("height"),
            "counted": "frames" if self.frames else "packets",
            "count": count,
            "per_s": count / self.seconds if count is not None and self.seconds else None,
        }
