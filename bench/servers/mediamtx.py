"""MediaMTX - the general-purpose contender.

Run against MediaMTX v1.21.3 (2026-11-15): every key below exists in that
version's sample mediamtx.yml, and all eleven ready scenarios ran. The runner
skips this contender while its binary is absent (OXRTC_MEDIAMTX).

Differences reported, not hidden (docs/BENCHMARKS.md):
- MediaMTX does not transcode: a transcoding viewer reads a path whose
  `runOnDemand` starts FFmpeg reading the copy path and publishing back - how
  an NVR would deploy it. Those FFmpeg processes are children of MediaMTX, so
  the tree sampler counts them.
- MediaMTX runs `runOnDemand` through its own command splitter (and a shell on
  Unix); the bench writes no user input into it.
- No preload API: a preload is an RTSP reader holding the path (clients/preload.py).
- WHEP lives on its own HTTP listener: `POST /<path>/whep`.
- When a source drops, MediaMTX closes the WebRTC sessions reading it
  ("[WebRTC] [session ...] closed: terminated"); go2rtc and oxrtc keep them
  open and resume. R01 and R02 therefore use a viewer that reconnects as
  openIVR's live view does, and MediaMTX's recovery includes that backoff.

Every listener is pinned to a free 127.0.0.1 port (the WebRTC UDP port to a
free port on every interface, as for go2rtc) and every protocol the bench does
not use is off: MediaMTX's defaults (8554, 8000, 8189, 1935, 8888, 8890)
include ports the owner's NVR uses on this machine.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import yaml

from common import binary, free_port, free_udp_port
from servers.base import Server


@dataclass
class Mediamtx(Server):
    webrtc_http_port: int = field(default_factory=free_port)
    rtp_port: int = field(default_factory=free_udp_port)
    rtcp_port: int = field(default_factory=free_udp_port)

    name = "mediamtx"
    binary_name = "mediamtx"
    tested = True
    provenance = "MediaMTX v1.21.3, run 2026-11-15"

    def paths(self, wants: set[str]) -> dict[str, dict]:
        ffmpeg = binary("ffmpeg") or "ffmpeg"
        paths: dict[str, dict] = {}
        for cam in self.cameras:
            copy = self.stream(cam.index)
            for name, url in ((copy, cam.main_url), (self.stream(cam.index, sub=True), cam.sub_url)):
                paths[name] = {"source": url, "rtspTransport": "tcp", "sourceOnDemand": True}
            for want in sorted(wants - {"copy"}):
                if want != "h264":
                    raise ValueError(f"mediamtx adapter: no transcode recipe for {want!r}")
                paths[self.stream(cam.index, want)] = {
                    "runOnDemand": (
                        f"{Path(ffmpeg).as_posix()} -hide_banner -loglevel error -rtsp_transport tcp"
                        f" -i rtsp://127.0.0.1:{self.rtsp_port}/{copy} -an -c:v libx264"
                        " -preset superfast -tune zerolatency -profile:v baseline -g 30 -bf 0"
                        f" -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:{self.rtsp_port}/$MTX_PATH"
                    ),
                    "runOnDemandRestart": True,
                }
        return paths

    def config(self, wants: set[str]) -> dict:
        return {
            "logLevel": "info",
            "logDestinations": ["stdout"],
            "authMethod": "internal",
            "authInternalUsers": [
                {"user": "any", "pass": "", "ips": ["127.0.0.1/32"],
                 "permissions": [{"action": "publish"}, {"action": "read"}, {"action": "api"}]},
            ],
            "api": True,
            "apiAddress": f"127.0.0.1:{self.api_port}",
            "metrics": False,
            "pprof": False,
            "playback": False,
            "rtsp": True,
            "rtspTransports": ["tcp"],
            "rtspEncryption": "no",
            "rtspAddress": f"127.0.0.1:{self.rtsp_port}",
            "rtpAddress": f"127.0.0.1:{self.rtp_port}",
            "rtcpAddress": f"127.0.0.1:{self.rtcp_port}",
            "rtmp": False,
            "hls": False,
            "srt": False,
            "webrtc": True,
            "webrtcAddress": f"127.0.0.1:{self.webrtc_http_port}",
            # every interface, as for go2rtc (servers/go2rtc.py says why)
            "webrtcLocalUDPAddress": f":{self.webrtc_port}",
            "webrtcLocalTCPAddress": "",
            "webrtcIPsFromInterfaces": True,
            "webrtcAdditionalHosts": ["127.0.0.1"],
            # No STUN, as for go2rtc: a public STUN server is not the server's speed.
            "webrtcICEServers2": [],
            "paths": self.paths(wants),
        }

    def config_name(self) -> str:
        return "mediamtx.yml"

    def config_text(self, wants: set[str]) -> str:
        return yaml.safe_dump(self.config(wants), sort_keys=False)

    def argv(self, config: Path) -> list[str]:
        return [self.binary_path() or self.binary_name, str(config)]

    def health_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}/v3/paths/list"

    def whep_url(self, stream: str) -> str:
        return f"http://127.0.0.1:{self.webrtc_http_port}/{stream}/whep"

    def version(self) -> str | None:
        from common import binary_version

        return binary_version(self.binary_path(), ["--version"])

    def describe(self) -> dict:
        facts = super().describe()
        facts["ports"].update(webrtc_http=self.webrtc_http_port, rtp=self.rtp_port, rtcp=self.rtcp_port)
        return facts


ADAPTER = Mediamtx
