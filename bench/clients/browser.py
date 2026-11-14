"""Python side of the WHEP browser probe: job file in, Node probe as a Child, result out.

`analyze()` turns the probe's raw frames into per-viewer figures with the
emitter's clock (cams/publisher.calibrate): glass-to-glass latency per frame
(mean, P95 - never a median), first frame, frame rate, code errors, and the
last getStats() counters.
"""

from __future__ import annotations

import json
import sys
import time
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import framecode  # noqa: E402
from common import Child, binary  # noqa: E402

PROBE = Path(__file__).resolve().parent / "whep_probe.mjs"


@dataclass
class Viewer:
    label: str
    url: str
    headers: dict[str, str]
    reference_width: int
    decode: bool = True
    cycles: int = 0
    hold_after_first_frame_ms: int = 1000
    gap_ms: int = 0

    def job(self) -> dict:
        return {
            "label": self.label, "url": self.url, "headers": self.headers,
            "referenceWidth": self.reference_width, "decode": self.decode,
            "cycles": self.cycles, "holdAfterFirstFrameMs": self.hold_after_first_frame_ms,
            "gapMs": self.gap_ms,
        }


class BrowserProbe:
    """One headless browser holding every WebRTC viewer of a run."""

    def __init__(self, workdir: Path, origin: str, viewers: list[Viewer], seconds: float,
                 channel: str = "msedge"):
        self.workdir = workdir
        self.job_path = workdir / "probe-job.json"
        self.out_path = workdir / "probe-out.json"
        self.seconds = seconds
        self.job = {
            "origin": origin, "channel": channel, "seconds": seconds,
            "viewers": [v.job() for v in viewers], "maxFramesPerViewer": 200_000,
        }
        self.child: Child | None = None

    @staticmethod
    def available() -> bool:
        return binary("node") is not None

    def start(self) -> "BrowserProbe":
        node = binary("node")
        if node is None:
            raise FileNotFoundError("node not found (set OXRTC_NODE)")
        self.workdir.mkdir(parents=True, exist_ok=True)
        self.job_path.write_text(json.dumps(self.job), encoding="utf-8")
        self.child = Child("browser", [node, str(PROBE), str(self.job_path), str(self.out_path)],
                           self.workdir / "browser.log").start()
        return self

    def wait(self, grace: float = 60) -> dict:
        """Wait for the probe to finish (seconds + grace), then read its output."""
        deadline = time.monotonic() + self.seconds + grace
        while self.child and self.child.alive() and time.monotonic() < deadline:
            time.sleep(0.5)
        if self.child:
            self.child.stop()
        try:
            return json.loads(self.out_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            tail = self.child.tail() if self.child else ""
            return {"error": f"probe wrote no output: {tail[-500:]}"}

    def stop(self) -> None:
        if self.child:
            self.child.stop()


def analyze_viewer(viewer: dict, e0: float | None, frame_ms: float, skip_s: float = 2.0) -> dict:
    """Figures of one viewer; latency only when the emitter's clock is known."""
    frames = viewer.get("frames") or []
    attempts = viewer.get("attempts") or []
    first = [a["firstFrameMs"] for a in attempts if a.get("firstFrameMs") is not None]
    out: dict = {
        "label": viewer.get("label"),
        "attempts": len(attempts),
        "failed_attempts": sum(1 for a in attempts if a.get("firstFrameMs") is None),
        "first_frame_ms": first,
        "frames_presented": len(frames),
        "error": viewer.get("error") or next((a.get("error") for a in attempts if a.get("error")), None),
    }
    if frames:
        span = (frames[-1][0] - frames[0][0]) / 1000
        out["presented_fps"] = (len(frames) - 1) / span if span > 0 else None
        codes = [f[1] for f in frames]
        decoded = [c for c in codes if c >= 0]
        if decoded:
            flags = framecode.consistent(codes)
            good = [f for f, ok in zip(frames, flags) if ok]
            out["code_frames"] = len(decoded)
            out["code_rejected"] = len(decoded) - len(good)
            start = frames[0][0] + skip_s * 1000
            steady = [f for f in good if f[0] >= start]
            if e0 is not None and steady:
                lat = [framecode.latency_ms(f[0], f[1], e0, frame_ms) for f in steady]
                out["latency_ms_mean"] = sum(lat) / len(lat)
                out["latency_ms_p95"] = framecode.percentile(lat, 95)
                out["latency_ms_max"] = max(lat)
                out["latency_samples"] = len(lat)
            # source frames skipped between consecutive presented frames
            gaps = [framecode.step(a[1], b[1]) for a, b in zip(good, good[1:])]
            out["source_frames_skipped"] = sum(g - 1 for g in gaps if g > 1)
    stats = [s for s in viewer.get("stats") or [] if s.get("kind") == "video"]
    if stats:
        last = stats[-1]
        out["video_stats_last"] = {k: last.get(k) for k in (
            "codec", "framesDecoded", "framesDropped", "freezeCount", "totalFreezesDuration",
            "packetsLost", "nackCount", "pliCount", "frameWidth", "frameHeight",
            "decoderImplementation", "jitterBufferDelay", "jitterBufferEmittedCount")}
        jb, n = last.get("jitterBufferDelay"), last.get("jitterBufferEmittedCount")
        if jb is not None and n:
            out["jitter_buffer_ms_mean"] = 1000 * jb / n
    return out
