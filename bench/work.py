"""What a run's contender did, beside what it cost: the work the tables compare (W6).

- `oxrtc_mode`: the oxrtc mode a run used, named on every row (the owner,
  2026-11-20: "chaque mode contre son pareil", each mode against its peer;
  D30). A slim run (bench/slim.py) keeps it, as `oxrtc_mode`, without the
  server's configuration it is read from.
"""

from __future__ import annotations

import json
import re

LIVE = "live (webrtc.lip_sync: false)"
#: oxrtc 0.1.x named a stream per track, which left the browser's lip sync off
#: whatever the configuration said (D30): its runs are told apart.
STREAM_PER_TRACK = "0.1.x: a stream per track, no lip sync"
LIP_SYNC = "lip sync on (default)"
NOT_RECORDED = "not recorded"


def oxrtc_mode(run: dict) -> str:
    """The oxrtc mode of a run, from its recorded configuration."""
    if run.get("oxrtc_mode"):
        return str(run["oxrtc_mode"])
    if re.search(r"(?<![\d.])0\.1\.\d+", str((run.get("server") or {}).get("version") or "")):
        return STREAM_PER_TRACK
    text = run.get("server_config")
    if not text:
        return NOT_RECORDED
    if not isinstance(text, str):
        text = json.dumps(text)
    mode = LIVE if re.search(r"\blip_sync\W+false", text) else LIP_SYNC
    lag = re.search(r"\bsound_lag_ms\W+(\d+)", text)
    return f"{mode}, sound_lag_ms {lag.group(1)}" if lag else mode


# -- what was delivered -------------------------------------------------------------

def run_error(run: dict) -> str | None:
    if run.get("status") == "ok":
        return None
    return (run.get("error") or "the run failed").replace("\n", " ")[:160]


def frames_delivered(run: dict) -> float | None:
    """Every picture the server handed its consumers over the run: each
    viewer's pictures decoded (its last `getStats()`) and each RTSP reader's
    count. None when no consumer counted any."""
    viewers = (run.get("browser") or {}).get("viewers") or []
    decoded = [(v.get("video_stats_last") or {}).get("framesDecoded") for v in viewers]
    read = [r.get("count") for r in run.get("readers") or []]
    counted = [n for n in decoded + read if isinstance(n, (int, float))]
    return float(sum(counted)) if counted and sum(counted) > 0 else None


def cpu_per_frame(key: str):
    """A getter: the CPU of `key` (a summary's "% of one core") per delivered
    frame, in ms of one core. A contender that delivers fewer pictures (the
    recorder of a webcam read at 25 a second where it sends 30) is charged for
    what it delivered, and one whose transcodes start late is charged for the
    pictures it showed, not the run's length."""

    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        pct = (run.get("summary") or {}).get(key)
        frames = frames_delivered(run)
        seconds = run.get("duration_s")
        if pct is None or not frames or not seconds:
            return None, "no figure"
        return 10.0 * pct * seconds / frames, None

    return get


def camera_frames_decoded(run: dict, camera: str) -> float | None:
    """The pictures one viewer of `camera` decoded over the run, its viewers' mean."""
    prefix = f"cam{camera.removeprefix('camera_')}-"
    counts = [(v.get("video_stats_last") or {}).get("framesDecoded")
              for v in (run.get("browser") or {}).get("viewers") or [] if (v.get("label") or "").startswith(prefix)]
    counts = [n for n in counts if isinstance(n, (int, float)) and n > 0]
    return sum(counts) / len(counts) if counts else None


def camera_cpu_per_picture(camera: str):
    """A getter: the CPU of `camera`'s FFmpeg children per picture its viewer
    decoded, ms of one core: a transcode's cost over the time it ran (RC4: go2rtc's
    webcam transcodes show their first picture 39 to 46 s into a 120 s run)."""

    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        cpu = (run.get("transcode_cpu_s_by_camera") or {}).get(camera)
        frames = camera_frames_decoded(run, camera)
        if cpu is None or not frames:
            return None, "no figure"
        return 1000.0 * cpu / frames, None

    return get


# -- while every transcode runs -----------------------------------------------------

def all_running(run: dict) -> dict | None:
    """The tree's CPU and memory over the samples where its FFmpeg processes are
    at their most (every transcode running): a contender whose transcodes start
    late is compared over the same state as one that starts at once, not over
    a run mean that charges the other for the time it showed a picture (D36,
    "Accepted"). A slim run (bench/slim.py) carries it as `work_all_running`."""
    if "work_all_running" in run:
        return run["work_all_running"]
    samples = [s for s in run.get("samples") or [] if "ffmpeg_procs" in s and "t" in s]
    if not samples:
        return None
    most = max(s["ffmpeg_procs"] for s in samples)
    pairs = [(a, b) for a, b in zip(samples, samples[1:]) if a["ffmpeg_procs"] == most == b["ffmpeg_procs"]]
    span = sum(b["t"] - a["t"] for a, b in pairs)
    if most == 0 or span <= 0:
        return None
    at_most = [s for s in samples if s["ffmpeg_procs"] == most]

    def cpu(key: str) -> float | None:
        if not all(key in a and key in b for a, b in pairs):
            return None
        return 100.0 * sum(b[key] - a[key] for a, b in pairs) / span

    def rss(key: str) -> float | None:
        values = [s[key] for s in at_most if key in s]
        return sum(values) / len(values) if values else None

    return {"ffmpeg_procs": most, "seconds": span, "cpu_pct_core": cpu("cpu_s"),
            "cpu_server_pct_core": cpu("cpu_server_s"), "cpu_children_pct_core": cpu("cpu_children_s"),
            "rss_mb": rss("rss_mb"), "rss_children_mb": rss("rss_children_mb")}


def all_running_value(key: str):
    """A getter: `key` of `all_running`."""

    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        value = (all_running(run) or {}).get(key)
        return (value, None) if value is not None else (None, "no figure")

    return get
