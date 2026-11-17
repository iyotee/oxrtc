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
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from common import Child, binary  # noqa: E402


def reader_label(cam: int, sub: bool, suffix: str, k: int) -> str:
    """An RTSP reader's label, which names its output file: unique per stream
    read. `cam{n}-{k}` alone gave B07's recorder (camera_N_rec) and AI reader
    (camera_N_sub) one file, and two ffprobe writing it garbled both."""
    return f"cam{cam}{'-sub' if sub else ''}{suffix.replace('_', '-')}-{k + 1}"


@dataclass
class RtspReader:
    label: str
    url: str
    seconds: float
    workdir: Path
    frames: bool = False
    #: Dial again, for the time that is left, when a read ends without a picture
    #: (a stream that was not ready yet): what a recorder does. Real-camera runs
    #: only - the synthetic ones start from a camera that is already there.
    retry: bool = False
    child: Child | None = field(default=None, repr=False)
    attempts: int = field(default=0, repr=False)
    _read_seconds: float = field(default=0.0, repr=False)
    _began: float = field(default=0.0, repr=False)
    _stopping: threading.Event = field(default_factory=threading.Event, repr=False)
    _supervisor: threading.Thread | None = field(default=None, repr=False)

    def argv(self, ffprobe: str, seconds: float | None = None) -> list[str]:
        count = "-count_frames" if self.frames else "-count_packets"
        entry = "stream=codec_name,width,height,nb_read_frames" if self.frames else \
            "stream=codec_name,width,height,nb_read_packets"
        return [
            ffprobe, "-v", "error", "-rtsp_transport", "tcp", "-timeout", "10000000",
            "-select_streams", "v:0", "-read_intervals", f"%+{(seconds or self.seconds):g}",
            count, "-show_entries", entry, "-of", "json", self.url,
        ]

    @property
    def out_path(self) -> Path:
        return self.workdir / f"reader-{self.label}.json"

    def _launch(self, seconds: float) -> None:
        ffprobe = binary("ffprobe")
        if ffprobe is None:
            raise FileNotFoundError("ffprobe not found (set OXRTC_FFPROBE)")
        self.attempts += 1
        self._read_seconds = seconds
        self.child = Child(f"reader-{self.label}", self.argv(ffprobe, seconds), self.out_path).start()

    def start(self) -> "RtspReader":
        self._began = time.monotonic()
        self._launch(self.seconds)
        if self.retry:
            self._supervisor = threading.Thread(target=self._supervise, name=f"reader-{self.label}", daemon=True)
            self._supervisor.start()
        return self

    def _read(self) -> tuple[dict, int | None]:
        """What the last read left in its file: (the first stream, its count or None)."""
        try:
            text = self.out_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return {}, None
        start = text.find("{")
        try:
            data = json.loads(text[start:]) if start >= 0 else {}
        except ValueError:
            return {}, None
        stream = (data.get("streams") or [{}])[0]
        try:
            return stream, int(stream.get("nb_read_frames" if self.frames else "nb_read_packets"))
        except (TypeError, ValueError):
            return stream, None

    def _supervise(self) -> None:
        """Restart a read that ended with no count while time is left (see `retry`)."""
        while not self._stopping.is_set():
            popen = self.child.popen if self.child else None
            if popen is None:
                return
            try:
                popen.wait(timeout=1.0)
            except Exception:  # noqa: BLE001 - still running
                continue
            left = self.seconds - (time.monotonic() - self._began)
            if self._read()[1] is not None or left < 15 or self._stopping.is_set():
                return
            self.child.stop()
            if self._stopping.wait(2.0):
                return
            self._launch(left - 2.0)

    def result(self) -> dict:
        """Stop if still running, parse ffprobe's JSON (stdout and stderr share the file)."""
        if self.child is None:
            return {"label": self.label, "error": "not started"}
        deadline = time.monotonic() + self.seconds + 15
        while time.monotonic() < deadline:
            popen = self.child.popen
            if popen is not None and popen.poll() is not None and not (
                    self._supervisor is not None and self._supervisor.is_alive()):
                break
            time.sleep(0.5)
        self._stopping.set()
        if self._supervisor is not None:
            self._supervisor.join(timeout=5)
        self.child.stop()
        text = self.out_path.read_text(encoding="utf-8", errors="replace")
        stream, count = self._read()
        if not stream:
            return {"label": self.label, "error": text[-300:], "attempts": self.attempts}
        seconds = self._read_seconds  # a restarted read counted only the time it had left
        return {
            "label": self.label,
            "codec": stream.get("codec_name"),
            "width": stream.get("width"),
            "height": stream.get("height"),
            "counted": "frames" if self.frames else "packets",
            "count": count,
            "attempts": self.attempts,
            "per_s": count / seconds if count is not None and seconds else None,
        }
