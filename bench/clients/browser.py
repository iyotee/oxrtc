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
    spread_ms: int = 0
    # Reconnect as openIVR's live view does (whep_probe.mjs, RECONNECT): for a
    # server that closes its viewers' sessions when the source drops.
    reconnect: bool = False

    def job(self) -> dict:
        return {
            "label": self.label, "url": self.url, "headers": self.headers,
            "referenceWidth": self.reference_width, "decode": self.decode,
            "cycles": self.cycles, "holdAfterFirstFrameMs": self.hold_after_first_frame_ms,
            "gapMs": self.gap_ms, "cycleDelaysMs": cycle_delays(self.cycles, self.spread_ms),
            "reconnect": self.reconnect,
        }


# The fractional part of i x the golden ratio fills [0, 1) evenly, and is the
# same sequence for every server: a reproducible spread, not a random one.
GOLDEN = 0.6180339887498949


def cycle_delays(cycles: int, spread_ms: int) -> list[int]:
    """Extra wait before each cycle's offer, spread evenly over `[0, spread_ms)`.

    Without it a cycle lasts first frame + hold + gap, so its length depends on
    the server, and the next offer lands at the same point of the camera's GOP
    every time: go2rtc's cycle in L03 was two GOPs to within 30 ms, each viewer
    arrived about 400 ms before a keyframe, and its cold start read 515 ms flat.
    With `spread_ms` set to the GOP, the offers sample every phase of it.
    """
    if cycles <= 0 or spread_ms <= 0:
        return []
    return [int(((i * GOLDEN) % 1.0) * spread_ms) for i in range(cycles)]


class BrowserProbe:
    """One headless browser holding every WebRTC viewer of a run."""

    def __init__(self, workdir: Path, origin: str, viewers: list[Viewer], seconds: float,
                 channel: str = "msedge", tile_width: int | None = None):
        self.workdir = workdir
        self.job_path = workdir / "probe-job.json"
        self.out_path = workdir / "probe-out.json"
        self.seconds = seconds
        self.job = {
            "origin": origin, "channel": channel, "seconds": seconds,
            "viewers": [v.job() for v in viewers], "maxFramesPerViewer": 200_000,
        }
        if tile_width:
            # Each viewer a tile of that width at 16:9 on a 1920x1080 page (RC5).
            self.job["tileWidth"] = int(tile_width)
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


def _per_unit(readings: list[dict], key: str) -> float | None:
    """`key` per emitted unit between the first and the last of `readings`, in ms."""
    if len(readings) < 2 or key not in readings[0] or key not in readings[-1]:
        return None
    emitted = readings[-1].get("jitterBufferEmittedCount", 0) - readings[0].get("jitterBufferEmittedCount", 0)
    return 1000 * (readings[-1][key] - readings[0][key]) / emitted if emitted > 0 else None


def lip_sync(stats: list[dict]) -> dict:
    """What the browser did with the sound and the picture over the second half of a viewer's readings.

    `sound_held_ms` / `picture_held_ms`: the audio and video jitter buffers per
    emitted unit. `picture_asked_ms`: what the browser adds to the picture's
    delay beyond what the network calls for (its target minus its minimum:
    the wait for the sound; near 0 when the picture is not held for it).
    `sound_behind_ms`: the picture's `estimatedPlayoutTimestamp` minus the
    sound's, averaged over the readings that have both (how far the sound is
    behind the picture for the viewer, by the server's clocks). The sound's
    concealed, removed and inserted samples are the whole run's.
    """
    if not stats:
        return {}
    half = (stats[0]["wall"] + stats[-1]["wall"]) / 2
    late = {kind: [s for s in stats if s.get("kind") == kind and s["wall"] >= half] for kind in ("video", "audio")}
    out: dict = {"sound_held_ms": _per_unit(late["audio"], "jitterBufferDelay"),
                 "picture_held_ms": _per_unit(late["video"], "jitterBufferDelay"),
                 "picture_asked_ms": None}
    target, least = (_per_unit(late["video"], key) for key in ("jitterBufferTargetDelay", "jitterBufferMinimumDelay"))
    if target is not None and least is not None:
        out["picture_asked_ms"] = target - least
    lags = []
    for picture in late["video"]:
        sound = next((a for a in late["audio"] if abs(a["wall"] - picture["wall"]) < 50), None)
        if sound and "estimatedPlayoutTimestamp" in picture and "estimatedPlayoutTimestamp" in sound:
            lags.append(picture["estimatedPlayoutTimestamp"] - sound["estimatedPlayoutTimestamp"])
    out["sound_behind_ms"] = sum(lags) / len(lags) if lags else None
    out["sound_behind_worst_ms"] = max(lags, key=abs) if lags else None
    sound = [s for s in stats if s.get("kind") == "audio"]
    if sound:
        last = sound[-1]
        total = last.get("totalSamplesReceived")
        for key in ("concealedSamples", "removedSamplesForAcceleration", "insertedSamplesForDeceleration",
                    "packetsReceived"):
            out[key] = last.get(key)
        if total and last.get("concealedSamples") is not None:
            out["sound_concealed_pct"] = 100 * last["concealedSamples"] / total
    return out


def av_offsets(frames: list, beeps: list[float], frame_ms: float, start_ms: float | None = None) -> dict:
    """Lip sync as the viewer saw and heard it: each beep's onset minus its flash's display.

    The synthetic camera flashes and beeps at the start of every second of
    its media (cams/media.py): the flash is the frame whose code is a whole
    second of frames. Each heard onset is paired with the displayed flash
    nearest to it, within 400 ms (one flash a second; the loop of the media
    puts two 840 ms apart). Positive: the sound comes after the picture.
    Unlike `lip_sync`'s `sound_behind_ms` this does not trust the dates a
    server gives its tracks. `start_ms`: only the pairs from then on.
    """
    fps = round(1000 / frame_ms) if frame_ms > 0 else 0
    codes = [f[1] for f in frames]
    flags = framecode.consistent(codes) if codes else []
    flashes = [f[0] for f, ok in zip(frames, flags) if ok and fps and f[1] >= 0 and f[1] % fps == 0]
    offsets = []
    for beep in beeps:
        if start_ms is not None and beep < start_ms:
            continue
        near = min(flashes, key=lambda at: abs(beep - at), default=None)
        if near is not None and abs(beep - near) < 400:
            offsets.append(beep - near)
    if not offsets:
        return {"av_pairs": 0}
    return {"av_pairs": len(offsets), "av_offset_ms": sum(offsets) / len(offsets),
            "av_offset_worst_ms": max(offsets, key=abs)}


def video_kbps(stats: list[dict], skip_s: float = 2.0) -> float | None:
    """The picture's bitrate as the viewer received it, kbit/s, after its first `skip_s` seconds.

    What a transcode produced (W6): x264 told a camera's rate encodes a fast
    webcam at about 7 Mbit/s, told none about 2.9 (D36), so a CPU row of a
    webcam's FFmpeg is read beside the bitrate its viewer received.
    """
    rows = [s for s in stats if s.get("bytesReceived") is not None and "wall" in s]
    if len(rows) < 2:
        return None
    start = rows[0]["wall"] + skip_s * 1000
    steady = [r for r in rows if r["wall"] >= start]
    if len(steady) < 2 or steady[-1]["wall"] <= steady[0]["wall"]:
        return None
    return 8 * (steady[-1]["bytesReceived"] - steady[0]["bytesReceived"]) / (steady[-1]["wall"] - steady[0]["wall"])


def analyze_viewer(viewer: dict, e0: float | None, frame_ms: float, skip_s: float = 2.0) -> dict:
    """Figures of one viewer; latency only when the emitter's clock is known."""
    frames = viewer.get("frames") or []
    # A reconnection (R01, R02) is recovery, not a first frame: its attempts,
    # refused while the source is down, are counted apart.
    every = viewer.get("attempts") or []
    attempts = [a for a in every if not a.get("reconnect")]
    first = [a["firstFrameMs"] for a in attempts if a.get("firstFrameMs") is not None]
    out: dict = {
        "label": viewer.get("label"),
        "attempts": len(attempts),
        "reconnect_attempts": len(every) - len(attempts),
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
            "decoderImplementation", "jitterBufferDelay", "jitterBufferEmittedCount",
            "framesReceived", "keyFramesDecoded", "firCount", "retransmittedPacketsReceived",
            "totalInterFrameDelay", "totalSquaredInterFrameDelay", "totalProcessingDelay")}
        jb, n = last.get("jitterBufferDelay"), last.get("jitterBufferEmittedCount")
        if jb is not None and n:
            out["jitter_buffer_ms_mean"] = 1000 * jb / n
        out["video_kbps"] = video_kbps(stats, skip_s)
    # What the viewer was sent, track by track (W6: the same tracks for every contender).
    out["tracks"] = sorted({f"{s['kind']}:{s['codec']}" for s in viewer.get("stats") or []
                            if s.get("kind") and s.get("codec")})
    out["lip_sync"] = lip_sync([s for s in viewer.get("stats") or [] if "wall" in s])
    beeps = viewer.get("beeps") or []
    if beeps and frames:
        # The second half, as `lip_sync`: a browser synchronises only once
        # it has both tracks' sender reports.
        out["lip_sync"].update(av_offsets(frames, beeps, frame_ms, (frames[0][0] + frames[-1][0]) / 2))
    if viewer.get("beepError"):
        out["lip_sync"]["beep_error"] = viewer["beepError"]
    return out
