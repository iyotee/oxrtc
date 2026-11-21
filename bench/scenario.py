"""Scenario files (scenarios/<ID>.yaml): loading, validation, and expansion into clients.

A scenario is data, never code. Its keys:

    id, title, measures           what docs/BENCHMARKS.md says it is
    status                        ready | needs-linux | not-implemented
    reason                        why, when status is not `ready`
    duration_s                    length of the measured window
    layout                        plain (default) | openivr: the streams openIVR
                                  writes (servers/go2rtc.py openivr_streams) |
                                  real: the same streams on the owner's real
                                  cameras (real.py); `cameras: {ids: [...]}`
                                  names them and their numbers are the camera
                                  numbers of every group below
    cameras: {count, profile}     profile = cams/media.Profile fields
    webrtc:  [{cameras, per_camera, want, sub, decode_first, cycles, hold_ms, gap_ms, spread_ms, reconnect,
               browsers, tile_px}]
             `browsers: N` (real layout): N browser processes, each holding a copy of the
             group's viewers (N people on N devices), tiles of `tile_px` at 16:9
    rtsp:    [{cameras, per_camera, sub, suffix, frames}]
    preloads: [{cameras, sub, suffix, queries}]

A `suffix` is appended to the camera's stream name (`_rec`: openIVR's
recording stream). `queries` are tried in order until one is not answered
500, as openIVR's hold_open() tries `audio=opus` then `video=`.
    faults:  [{at_s, cameras, down_s}]   camera outages (R01, R02)

`cameras` in a group is `all` or a list of camera numbers (1-based).
"""

from __future__ import annotations

import sys
from dataclasses import fields
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

from cams.media import Profile  # noqa: E402

SCENARIOS = Path(__file__).resolve().parent / "scenarios"
STATUSES = {"ready", "needs-linux", "not-implemented"}
TOP_KEYS = {"id", "title", "measures", "status", "reason", "duration_s", "cameras",
            "webrtc", "rtsp", "preloads", "faults", "notes", "layout"}
LAYOUTS = {"plain", "openivr", "real"}
SUFFIXES = {"", "_rec", "_src"}
AUDIOS = {"pcma", "pcma16", "none"}
WANTS = {"copy", "h264"}
MAX_CAMERAS = 64
MAX_VIEWERS = 256
#: The most browser processes a group may ask for: the owner's real case is five.
MAX_BROWSERS = 8


class ScenarioError(ValueError):
    pass


def load(scenario_id: str) -> dict:
    path = SCENARIOS / f"{scenario_id}.yaml"
    if not path.is_file():
        raise ScenarioError(f"no scenario {scenario_id!r} in {SCENARIOS}")
    data = yaml.safe_load(path.read_text(encoding="utf-8"))
    validate(data)
    if data["id"] != scenario_id:
        raise ScenarioError(f"{path.name}: id {data['id']!r} does not match the file name")
    return data


def all_ids() -> list[str]:
    return sorted(p.stem for p in SCENARIOS.glob("*.yaml"))


def profile(data: dict) -> Profile:
    allowed = {f.name for f in fields(Profile)}
    raw = (data.get("cameras") or {}).get("profile") or {}
    unknown = set(raw) - allowed
    if unknown:
        raise ScenarioError(f"unknown profile keys {sorted(unknown)}")
    if raw.get("audio", "pcma") not in AUDIOS:
        raise ScenarioError(f"profile audio must be one of {sorted(AUDIOS)}")
    return Profile(**{k: (str(v) if k == "fps" else v) for k, v in raw.items()})


def layout(data: dict) -> str:
    return data.get("layout", "plain")


def real_ids(data: dict) -> list[int]:
    """The real cameras' numbers of a `real` scenario (empty for every other layout)."""
    return list((data.get("cameras") or {}).get("ids") or [])


def camera_list(group: dict, count: int, ids: list[int] | None = None) -> list[int]:
    """The camera numbers a group addresses: 1..count, or `ids` for the real cameras."""
    chosen = group.get("cameras", "all")
    universe = list(ids) if ids else list(range(1, count + 1))
    if chosen == "all":
        return universe
    if not isinstance(chosen, list) or not all(isinstance(c, int) and c in universe for c in chosen):
        raise ScenarioError(f"cameras must be 'all' or numbers within {universe}: {chosen!r}")
    return chosen


def validate(data: dict) -> None:
    if not isinstance(data, dict):
        raise ScenarioError("a scenario is a mapping")
    unknown = set(data) - TOP_KEYS
    if unknown:
        raise ScenarioError(f"unknown keys {sorted(unknown)}")
    for key in ("id", "title", "status", "measures"):
        if key not in data:
            raise ScenarioError(f"missing {key!r}")
    if data["status"] not in STATUSES:
        raise ScenarioError(f"status must be one of {sorted(STATUSES)}")
    if data["status"] != "ready" and not data.get("reason"):
        raise ScenarioError("a scenario that is not ready says why (reason)")
    if data["status"] == "not-implemented":
        return
    count = (data.get("cameras") or {}).get("count", 0)
    if not isinstance(count, int) or not 1 <= count <= MAX_CAMERAS:
        raise ScenarioError(f"cameras.count must be 1..{MAX_CAMERAS}")
    if not isinstance(data.get("duration_s"), (int, float)) or data["duration_s"] <= 0:
        raise ScenarioError("duration_s must be a positive number")
    profile(data)
    if layout(data) not in LAYOUTS:
        raise ScenarioError(f"layout must be one of {sorted(LAYOUTS)}")
    ids = real_ids(data)
    if layout(data) == "real":
        if not ids or not all(isinstance(i, int) and i > 0 for i in ids) or len(set(ids)) != len(ids):
            raise ScenarioError("a real scenario names its cameras: cameras.ids, distinct positive numbers")
        if count != len(ids):
            raise ScenarioError("cameras.count must be the number of cameras.ids")
        if data.get("faults"):
            raise ScenarioError("a real scenario has no faults: the cameras are not ours to cut")
    elif ids:
        raise ScenarioError("cameras.ids belongs to the real layout")
    for group in (data.get("rtsp") or []) + (data.get("preloads") or []):
        if group.get("suffix", "") not in SUFFIXES:
            raise ScenarioError(f"suffix must be one of {sorted(SUFFIXES)}")
    for group in data.get("preloads") or []:
        queries = group.get("queries", [""])
        if not isinstance(queries, list) or not queries or not all(isinstance(q, str) for q in queries):
            raise ScenarioError("preload queries are a non-empty list of strings")
    viewers = 0
    for group in data.get("webrtc") or []:
        if group.get("want", "copy") not in WANTS:
            raise ScenarioError(f"want must be one of {sorted(WANTS)}")
        browsers = group.get("browsers", 0)
        if not isinstance(browsers, int) or not 0 <= browsers <= MAX_BROWSERS:
            raise ScenarioError(f"browsers must be 0..{MAX_BROWSERS}")
        if browsers and layout(data) != "real":
            raise ScenarioError("browsers belongs to the real layout")
        tile = group.get("tile_px", 0)
        if not isinstance(tile, int) or not 0 <= tile <= 1920:
            raise ScenarioError("tile_px must be 0..1920")
        viewers += len(camera_list(group, count, ids)) * int(group.get("per_camera", 1)) * max(1, browsers)
    for group in data.get("rtsp") or []:
        viewers += len(camera_list(group, count, ids)) * int(group.get("per_camera", 1))
    for group in data.get("preloads") or []:
        camera_list(group, count, ids)
    for fault in data.get("faults") or []:
        camera_list(fault, count)
        if fault.get("down_s", 0) <= 0 or fault.get("at_s", -1) < 0:
            raise ScenarioError("a fault has at_s >= 0 and down_s > 0")
    if viewers > MAX_VIEWERS:
        raise ScenarioError(f"{viewers} consumers: more than {MAX_VIEWERS}")


def wants(data: dict) -> set[str]:
    return {"copy"} | {g.get("want", "copy") for g in data.get("webrtc") or []}
