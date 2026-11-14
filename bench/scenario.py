"""Scenario files (scenarios/<ID>.yaml): loading, validation, and expansion into clients.

A scenario is data, never code. Its keys:

    id, title, measures           what docs/BENCHMARKS.md says it is
    status                        ready | needs-linux | not-implemented
    reason                        why, when status is not `ready`
    duration_s                    length of the measured window
    cameras: {count, profile}     profile = cams/media.Profile fields
    webrtc:  [{cameras, per_camera, want, sub, decode_first, cycles, hold_ms, gap_ms, spread_ms}]
    rtsp:    [{cameras, per_camera, sub, frames}]
    preloads: [{cameras, sub}]
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
            "webrtc", "rtsp", "preloads", "faults", "notes"}
WANTS = {"copy", "h264"}
MAX_CAMERAS = 64
MAX_VIEWERS = 256


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
    return Profile(**{k: (str(v) if k == "fps" else v) for k, v in raw.items()})


def camera_list(group: dict, count: int) -> list[int]:
    chosen = group.get("cameras", "all")
    if chosen == "all":
        return list(range(1, count + 1))
    if not isinstance(chosen, list) or not all(isinstance(c, int) and 1 <= c <= count for c in chosen):
        raise ScenarioError(f"cameras must be 'all' or numbers within 1..{count}: {chosen!r}")
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
    viewers = 0
    for group in data.get("webrtc") or []:
        if group.get("want", "copy") not in WANTS:
            raise ScenarioError(f"want must be one of {sorted(WANTS)}")
        viewers += len(camera_list(group, count)) * int(group.get("per_camera", 1))
    for group in data.get("rtsp") or []:
        viewers += len(camera_list(group, count)) * int(group.get("per_camera", 1))
    for group in data.get("preloads") or []:
        camera_list(group, count)
    for fault in data.get("faults") or []:
        camera_list(fault, count)
        if fault.get("down_s", 0) <= 0 or fault.get("at_s", -1) < 0:
            raise ScenarioError("a fault has at_s >= 0 and down_s > 0")
    if viewers > MAX_VIEWERS:
        raise ScenarioError(f"{viewers} consumers: more than {MAX_VIEWERS}")


def wants(data: dict) -> set[str]:
    return {"copy"} | {g.get("want", "copy") for g in data.get("webrtc") or []}
