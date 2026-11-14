"""What every contender adapter provides: config, start, stop, health, endpoints.

A scenario speaks of *cameras* (camera N: main and substream on the camera
server) and of what a viewer wants (`copy`, or a transcode to `h264`). Each
adapter maps those onto its own server's naming: the stream names it
configures, the WHEP URL a browser posts its offer to, the RTSP URL a reader
opens, and how a stream is held open without a viewer (a preload).
"""

from __future__ import annotations

import base64
import json
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

from common import Child, binary, binary_version, free_port, free_udp_port, wait_until


@dataclass
class Camera:
    """Camera N as the server under test sees it: the camera server's URLs."""

    index: int
    main_url: str
    sub_url: str


@dataclass
class Server:
    """Base adapter. Subclasses set `name`, `binary_name`, `tested` and the hooks."""

    workdir: Path
    cameras: list[Camera]
    api_port: int = field(default_factory=free_port)
    rtsp_port: int = field(default_factory=free_port)
    webrtc_port: int = field(default_factory=free_udp_port)
    child: Child | None = None

    name = "base"
    binary_name = "base"
    #: False until the adapter has run against the real binary once.
    tested = False
    #: How the adapter was written, for the report (documentation it follows).
    provenance = ""
    #: Credentials of the API, when the server requires them (never real ones).
    username: str | None = None
    password: str | None = None

    def __post_init__(self) -> None:
        """Hook for subclasses (credentials); the dataclass __init__ calls it."""

    # -- facts -------------------------------------------------------------
    @classmethod
    def binary_path(cls) -> str | None:
        return binary(cls.binary_name)

    @classmethod
    def available(cls) -> bool:
        return cls.binary_path() is not None

    def version(self) -> str | None:
        return binary_version(self.binary_path(), ["-version"])

    # -- naming ------------------------------------------------------------
    def stream(self, camera: int, want: str = "copy", sub: bool = False) -> str:
        """The stream name a viewer of camera N asks for."""
        base = f"camera_{camera}" + ("_sub" if sub else "")
        return base if want == "copy" else f"{base}_{want}"

    def whep_url(self, stream: str) -> str:
        raise NotImplementedError

    def rtsp_url(self, stream: str) -> str:
        return f"rtsp://127.0.0.1:{self.rtsp_port}/{stream}"

    def auth_header(self) -> dict[str, str]:
        if self.username is None:
            return {}
        token = base64.b64encode(f"{self.username}:{self.password}".encode()).decode()
        return {"Authorization": f"Basic {token}"}

    # -- lifecycle -----------------------------------------------------------
    def config_text(self, wants: set[str]) -> str:
        raise NotImplementedError

    def config_name(self) -> str:
        return f"{self.name}.yaml"

    def argv(self, config: Path) -> list[str]:
        return [self.binary_path() or self.binary_name, "-config", str(config)]

    def health_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}/api"

    def start(self, wants: set[str]) -> "Server":
        if not self.available():
            raise FileNotFoundError(f"{self.name}: binary {self.binary_name} not found")
        self.workdir.mkdir(parents=True, exist_ok=True)
        path = self.workdir / self.config_name()
        path.write_text(self.config_text(wants), encoding="utf-8")
        self.child = Child(self.name, self.argv(path), self.workdir / f"{self.name}.log",
                           cwd=self.workdir).start()
        if not wait_until(self.healthy, 15):
            tail = self.child.tail()
            self.stop()
            raise RuntimeError(f"{self.name} not healthy within 15 s:\n{tail}")
        return self

    def healthy(self) -> bool:
        return self.request("GET", self.health_url())[0] == 200

    def stop(self) -> None:
        if self.child is not None:
            self.child.stop()

    @property
    def pid(self) -> int:
        if self.child is None:
            raise RuntimeError(f"{self.name} not started")
        return self.child.pid

    # -- preload -------------------------------------------------------------
    def supports_preload_api(self) -> bool:
        return False

    def preload(self, stream: str) -> int:
        """Hold `stream` open with no viewer; HTTP status (only when supported)."""
        raise NotImplementedError

    # -- helpers -------------------------------------------------------------
    def request(self, method: str, url: str, body: bytes | None = None,
                headers: dict[str, str] | None = None, timeout: float = 5) -> tuple[int, bytes]:
        req = urllib.request.Request(url, data=body, method=method,
                                     headers={**self.auth_header(), **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 - loopback
                return resp.status, resp.read()
        except urllib.error.HTTPError as err:
            return err.code, err.read()
        except (urllib.error.URLError, OSError):
            return 0, b""

    def describe(self) -> dict:
        """What goes into the run's JSON about this contender."""
        return {
            "name": self.name,
            "binary": self.binary_path(),
            "version": self.version(),
            "tested_adapter": self.tested,
            "provenance": self.provenance,
            "ports": {"api": self.api_port, "rtsp": self.rtsp_port, "webrtc": self.webrtc_port},
        }

    def streams_info(self) -> object:
        status, body = self.request("GET", f"http://127.0.0.1:{self.api_port}/api/streams")
        if status != 200:
            return None
        try:
            return json.loads(body.decode("utf-8"))
        except ValueError:
            return None
