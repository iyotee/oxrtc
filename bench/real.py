"""The owner's real cameras: where their sources are read from, who answers, what is scrubbed.

The real-camera scenarios (RC1-RC4) give each contender openIVR's exact stream
layout for the cameras the owner has configured. The source lines are read from
openIVR's own generated media-server config at run time (`config/go2rtc.yaml`,
or `config/oxrtc.yaml` once openIVR runs oxrtc) and live in memory only: they
are never written into the repository, into a scenario file, or into a result.
Every text the run records goes through `scrub()` first, which replaces each
source URL, its address and any credential in it by `<camera_N>`.

The cameras are fragile. The XM ones ration RTSP connections and refuse a probe
while they stream, so a run holds at most one session per camera (the server
under test's own), contenders run one at a time, and `preflight()` is a single
TCP connect, not a media session. A camera that does not answer the preflight
(an ESP32 that is switched off) stays in the server's configuration, as it does
in openIVR, and gets no client: its row says "unreachable", for every contender.
"""

from __future__ import annotations

import os
import re
import socket
import sys
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import REPO  # noqa: E402

#: Default location of openIVR's config: the repository beside this one.
DEFAULT_CONFIG_DIR = REPO.parent / "openIVR" / "config"
#: The generated media-server configs, in the order they are looked for.
CONFIG_NAMES = ("go2rtc.yaml", "oxrtc.yaml")


class RealCameraError(RuntimeError):
    pass


def config_dir() -> Path:
    return Path(os.environ.get("OXRTC_OPENIVR_CONFIG") or DEFAULT_CONFIG_DIR)


def load_config() -> dict:
    """openIVR's generated media-server config, as a mapping."""
    for name in CONFIG_NAMES:
        path = config_dir() / name
        if path.is_file():
            return yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    raise RealCameraError(f"none of {', '.join(CONFIG_NAMES)} in {config_dir()} (set OXRTC_OPENIVR_CONFIG)")


def ffmpeg_h264_template(config: dict) -> str | None:
    """openIVR's own `ffmpeg: h264:` encoder arguments (not a secret), or None."""
    value = (config.get("ffmpeg") or {}).get("h264")
    return str(value) if value else None


@dataclass
class RealCamera:
    """One of the owner's cameras. `line` is its source as openIVR's config writes it."""

    id: int
    line: str
    url: str = field(init=False)
    scheme: str = field(init=False)
    host: str = field(init=False)
    port: int = field(init=False)
    username: str | None = field(init=False, default=None)
    password: str | None = field(init=False, default=None)

    def __post_init__(self) -> None:
        self.url = self.line.split("#", 1)[0]
        parts = urlsplit(self.url)
        self.scheme = parts.scheme
        self.host = parts.hostname or ""
        self.port = parts.port or {"rtsp": 554, "http": 80, "https": 443}.get(parts.scheme, 0)
        self.username = parts.username
        self.password = parts.password

    @property
    def name(self) -> str:
        return f"camera_{self.id}"

    def secrets(self) -> list[str]:
        """Every string of this camera that must not reach a result, longest first."""
        found = {self.line, self.url, self.host}
        if self.username:
            found.add(self.username)
        if self.password:
            found.add(self.password)
        return sorted((s for s in found if s), key=len, reverse=True)

    def reachable(self, timeout: float = 3.0) -> bool:
        """One TCP connect to the camera's port: no media session is opened."""
        if not self.host or not self.port:
            return False
        try:
            with socket.create_connection((self.host, self.port), timeout=timeout):
                return True
        except OSError:
            return False


def load_cameras(ids: list[int], config: dict | None = None) -> list[RealCamera]:
    """The cameras `ids`, each with the source line of its `camera_<id>_src` stream."""
    config = config if config is not None else load_config()
    streams = config.get("streams") or {}
    cameras = []
    for cid in ids:
        sources = streams.get(f"camera_{cid}_src")
        if not sources:
            raise RealCameraError(f"openIVR's config has no stream camera_{cid}_src")
        line = sources[0] if isinstance(sources, list) else sources
        cameras.append(RealCamera(cid, str(line)))
    return cameras


def scrub(text: str, cameras: list[RealCamera]) -> str:
    """`text` with every camera's source, address and credentials replaced by `<camera_N>`."""
    pairs = sorted(((s, cam.name) for cam in cameras for s in cam.secrets()), key=lambda p: len(p[0]), reverse=True)
    for secret, name in pairs:
        text = text.replace(secret, f"<{name}>")
    return text


def scrub_obj(obj: object, cameras: list[RealCamera]) -> object:
    """`scrub` applied to every string of a JSON-like structure (keys included)."""
    if isinstance(obj, str):
        return scrub(obj, cameras)
    if isinstance(obj, list):
        return [scrub_obj(v, cameras) for v in obj]
    if isinstance(obj, dict):
        return {scrub(str(k), cameras): scrub_obj(v, cameras) for k, v in obj.items()}
    return obj


def leaks(text: str, cameras: list[RealCamera]) -> list[str]:
    """The camera names whose secrets still appear in `text` (a check, run on what is written)."""
    return [cam.name for cam in cameras if any(s in text for s in cam.secrets())]


#: Kinds shown in the report, never derived from an address.
_IPV4 = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")


def has_address(text: str) -> bool:
    """True when `text` still holds a dotted IPv4 address other than loopback or a version string.

    A coarse second check on top of `leaks()`: an address that the camera list
    did not know (a redirect target in an error message) is caught here.
    """
    for found in _IPV4.findall(text):
        if found.startswith(("127.", "0.0.0.", "255.")):
            continue
        return True
    return False
