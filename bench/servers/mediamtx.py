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

With `layout: real` (RC1-RC4: the owner's real cameras) MediaMTX gets the
equivalent of openIVR's streams through its own means, never go2rtc's:
- `camera_N_src`: an RTSP source is a native `source:` path (on demand); an
  HTTP(S) MJPEG webcam is something MediaMTX cannot read (it has no HTTP/MJPEG
  source), so its path is published by an FFmpeg `runOnDemand` that copies the
  JPEG frames into RTSP - one more hop that go2rtc and oxrtc do not have -, each
  picture dated when it arrives (`-use_wallclock_as_timestamps`, as go2rtc and
  oxrtc date them: undated, FFmpeg stamped them 1/25 s apart);
- `camera_N_rec`: for an RTSP camera a `runOnDemand` FFmpeg, the camera's video
  copied, its sound to Opus (go2rtc's `ffmpeg:...#audio=opus`); for a webcam,
  which has no sound, `camera_N_src` read back as a `source:` (no FFmpeg, as
  go2rtc and oxrtc run none);
- `camera_N`: `runOnDemand` FFmpeg, video to H.264 with openIVR's own live
  template (its encoder arguments and its scale: the camera's size up to 1080p's
  pixel count), sound to Opus (go2rtc's `ffmpeg:...#video=h264/live`).
- there is no preload API: openIVR's hold of `camera_N_rec` is an RTSP reader.

Every listener is pinned to a free 127.0.0.1 port (the WebRTC UDP port to a
free port on every interface, as for go2rtc) and every protocol the bench does
not use is off: MediaMTX's defaults (8554, 8000, 8189, 1935, 8888, 8890)
include ports the owner's NVR uses on this machine.
"""

from __future__ import annotations

import shlex
from dataclasses import dataclass, field
from pathlib import Path

import yaml

from common import binary, free_port, free_udp_port
from servers.base import Server
from servers.go2rtc import LIVE_TEMPLATE


#: How long MediaMTX waits for an on-demand source or command to publish. openIVR waits
#: 30 s for a camera and go2rtc answers a slow one after 28 s; MediaMTX's default is 10 s,
#: which the owner's one-picture-a-second webcam does not meet (RC1 smoke, 2026-11-17).
START_TIMEOUT = "30s"


def _quoted(arg: str) -> str:
    """One argument of a `runOnDemand` line: in double quotes when it holds what a
    command line would split or read (the live scale's commas, parentheses and quotes)."""
    if arg and not any(c in arg for c in " \t'(),*;&|<>^%"):
        return arg
    return f'"{arg}"'


@dataclass
class Mediamtx(Server):
    webrtc_http_port: int = field(default_factory=free_port)
    rtp_port: int = field(default_factory=free_udp_port)
    rtcp_port: int = field(default_factory=free_udp_port)

    name = "mediamtx"
    binary_name = "mediamtx"
    tested = True
    provenance = "MediaMTX v1.21.3, run 2026-11-15"

    layouts = frozenset({"plain", "real"})

    def _ffmpeg_publish(self, inputs: list[str], codec_args: list[str]) -> str:
        """An FFmpeg command line that reads `inputs` and publishes to its own MediaMTX path."""
        ffmpeg = Path(binary("ffmpeg") or "ffmpeg").as_posix()
        argv = [ffmpeg, "-hide_banner", "-loglevel", "error", *inputs, *codec_args, "-f", "rtsp",
                "-rtsp_transport", "tcp", f"rtsp://127.0.0.1:{self.rtsp_port}/$MTX_PATH"]
        return " ".join(_quoted(arg) for arg in argv)

    def real_paths(self) -> dict[str, dict]:
        """The paths of `layout: real`; see the module docstring."""
        # openIVR's live template (go2rtc's `#video=h264/live`): its encode and its
        # scale, which keeps the camera's own size up to 1080p's pixel count.
        live = shlex.split(self.live_template or LIVE_TEMPLATE)
        opus = ["-c:a", "libopus", "-b:a", "32k", "-ac", "1"]
        paths: dict[str, dict] = {}
        for cam in self.cameras:
            name = f"camera_{cam.index}"
            src, rec = f"{name}_src", f"{name}_rec"
            url = cam.main_url.split("#", 1)[0]
            if url.startswith("rtsp"):
                paths[src] = {"source": url, "rtspTransport": "tcp", "sourceOnDemand": True,
                              "sourceOnDemandStartTimeout": START_TIMEOUT}
            else:
                # A short probe: a webcam sending one picture a second would otherwise
                # make FFmpeg wait several pictures before it publishes the first.
                # Each picture dated when it arrives, as go2rtc and oxrtc date a
                # webcam's: a Motion JPEG stream carries no dates, and FFmpeg's demuxer
                # stamped them 1/25 s apart - the recorder read 25 pictures a second of
                # a webcam that sends 30, and a one-a-second webcam's 300 s took 7500 s
                # of its clock (W6).
                paths[src] = {
                    "runOnDemand": self._ffmpeg_publish(
                        ["-use_wallclock_as_timestamps", "1", "-analyzeduration", "1000000", "-probesize", "500000",
                         "-i", url], ["-an", "-c:v", "copy"]),
                    "runOnDemandRestart": True,
                    "runOnDemandStartTimeout": START_TIMEOUT,
                }
            local = ["-rtsp_transport", "tcp", "-i", f"rtsp://127.0.0.1:{self.rtsp_port}/{src}"]
            if url.startswith("rtsp"):
                # The camera's picture relayed, its sound to Opus: an FFmpeg, as the
                # sound is go2rtc's and was oxrtc's before W5.
                paths[rec] = {
                    "runOnDemand": self._ffmpeg_publish(local, ["-map", "0:v", "-c:v", "copy", "-map", "0:a?", *opus]),
                    "runOnDemandRestart": True,
                    "runOnDemandStartTimeout": START_TIMEOUT,
                }
            else:
                # A webcam has no sound: its recorder's stream is the picture relayed,
                # MediaMTX's own path read back, with no FFmpeg (go2rtc and oxrtc run
                # none for it either; an FFmpeg that only copied cost MediaMTX a process
                # per webcam, W6).
                paths[rec] = {"source": f"rtsp://127.0.0.1:{self.rtsp_port}/{src}", "rtspTransport": "tcp",
                              "sourceOnDemand": True, "sourceOnDemandStartTimeout": START_TIMEOUT}
            paths[name] = {
                "runOnDemand": self._ffmpeg_publish(
                    local, ["-map", "0:v", *live, "-map", "0:a?", *opus]),
                "runOnDemandRestart": True,
                "runOnDemandStartTimeout": START_TIMEOUT,
            }
        return paths

    def paths(self, wants: set[str]) -> dict[str, dict]:
        if self.layout == "real":
            return self.real_paths()
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
