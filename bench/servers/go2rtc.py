"""go2rtc 1.9.15 - the reference: openIVR runs it in production today.

Configured the way openIVR configures it (docs/COMPATIBILITY.md): one stream
per camera pulling the camera's main RTSP over TCP, one for its substream, and
for a transcoding viewer an `ffmpeg:` stream on top of the copy stream. WHEP is
`POST /api/webrtc?src=<stream>` with the SDP offer as the body; a preload is
`PUT /api/preload?src=<stream>`.

With `layout: real` (RC1-RC4) the same streams are written for the owner's real
cameras: `camera_N_src` is the source line of openIVR's own config, verbatim,
and there is no substream (his cameras have none configured).

With `layout: openivr` (B07), the streams are the ones openIVR's
go2rtc_manager.py writes (openivr_streams), copied from its
`recording_sources()` and `derived_sources()` without the de-duplicated sound
source (an openIVR service, absent when it is not running).
"""

from __future__ import annotations

import yaml

from servers.base import Server

#: openIVR's app/services/streaming/patience.py.
CAMERA_STALL_SECONDS = 15
RELAY_STALL_SECONDS = 20

#: openIVR's app/services/streaming/media_server_manager.py (held to it by
#: tests/test_layout.py): the H.264 encode of every transcode, and since
#: 2026-11-22 the live transcode's template, `h264/live`, the same encode with a
#: scale that keeps the camera's own size up to 1920x1080's pixel count and
#: shrinks above it with the camera's shape (it said `#width=1920#height=1080`,
#: which enlarged a small webcam to 1080p).
H264_ENCODE = (
    "-codec:v libx264 -g:v 30 -preset:v superfast "
    "-tune:v zerolatency -profile:v baseline -level:v 5.1 "
    "-pix_fmt:v yuv420p -fps_mode:v passthrough"
)
LIVE_PIXEL_BUDGET = 1920 * 1080
LIVE_H264 = "h264/live"
LIVE_SCALE = (
    f"scale='if(gt(iw*ih,{LIVE_PIXEL_BUDGET}),"
    f"trunc(iw*sqrt({LIVE_PIXEL_BUDGET}/(iw*ih))/2)*2,trunc(iw/2)*2)':-2"
)
LIVE_TEMPLATE = f'{H264_ENCODE} -vf "{LIVE_SCALE}"'


def openivr_streams(index: int, main_url: str, sub_url: str, rtsp_port: int) -> dict[str, list[str]]:
    """openIVR's streams for camera `index`, as its go2rtc_manager.py writes them."""
    name = f"camera_{index}"
    src = f"{name}_src"
    video = f"rtsp://127.0.0.1:{rtsp_port}/{src}?video#timeout={RELAY_STALL_SECONDS}"
    return {
        src: [f"{main_url}#timeout={CAMERA_STALL_SECONDS}"],
        f"{name}_sub": [f"{sub_url}#timeout={CAMERA_STALL_SECONDS}"],
        f"{name}_rec": [
            video,
            f"ffmpeg:rtsp://127.0.0.1:{rtsp_port}/{src}#audio=opus#timeout={RELAY_STALL_SECONDS}",
        ],
        name: [
            video,
            f"ffmpeg:{src}#video={LIVE_H264}#timeout={RELAY_STALL_SECONDS}",
            f"rtsp://127.0.0.1:{rtsp_port}/{name}_rec?audio=opus#timeout={RELAY_STALL_SECONDS}",
        ],
    }


def real_streams(index: int, source_line: str, rtsp_port: int) -> dict[str, list[str]]:
    """openIVR's streams for a REAL camera: its `camera_N_src` line, then the derived ones.

    The derived streams are `openivr_streams`' own (one implementation); only
    the camera and its absent substream differ.
    """
    streams = openivr_streams(index, "", "", rtsp_port)
    streams[f"camera_{index}_src"] = [source_line]
    del streams[f"camera_{index}_sub"]
    return streams


class Go2rtc(Server):
    name = "go2rtc"
    binary_name = "go2rtc"
    tested = True
    provenance = "go2rtc README (1.9.x) and openIVR's go2rtc_manager.py; run on this machine"
    layouts = frozenset({"plain", "openivr", "real"})

    def streams(self, wants: set[str]) -> dict[str, list[str]]:
        streams: dict[str, list[str]] = {}
        if self.layout == "real":
            for cam in self.cameras:
                streams.update(real_streams(cam.index, cam.main_url, self.rtsp_port))
            return streams
        if self.layout == "openivr":
            for cam in self.cameras:
                streams.update(openivr_streams(cam.index, cam.main_url, cam.sub_url, self.rtsp_port))
            return streams
        for cam in self.cameras:
            copy = self.stream(cam.index)
            streams[copy] = [f"{cam.main_url}#timeout=15"]
            streams[self.stream(cam.index, sub=True)] = [f"{cam.sub_url}#timeout=15"]
            for want in sorted(wants - {"copy"}):
                streams[self.stream(cam.index, want)] = [f"ffmpeg:{copy}#video={want}"]
        return streams

    def config(self, wants: set[str]) -> dict:
        config = {
            "api": {"listen": f"127.0.0.1:{self.api_port}"},
            "rtsp": {"listen": f"127.0.0.1:{self.rtsp_port}"},
            # No STUN: go2rtc's default ice_servers (a public STUN server) make it
            # gather a server-reflexive candidate before answering - 5 s measured
            # on every WHEP POST on loopback, which is not the server's speed.
            # The WebRTC port listens on every interface, as openIVR deploys it:
            # bound to 127.0.0.1 alone, ICE never completed (go2rtc 1.9.15 with
            # Edge 157, loopback candidates offered on both sides, 2026-11-14).
            "webrtc": {"listen": f":{self.webrtc_port}", "candidates": [f"127.0.0.1:{self.webrtc_port}"],
                       "ice_servers": []},
            "srtp": {"listen": ""},
            "log": {"level": "info"},
            "streams": self.streams(wants),
        }
        from common import binary

        ffmpeg = binary("ffmpeg")
        if ffmpeg:
            config["ffmpeg"] = {"bin": ffmpeg}
        if self.layout in ("real", "openivr"):
            # openIVR writes both templates into its server's `ffmpeg:` section; the
            # real layout takes them from its config, B07 from the copy above.
            templates = config.setdefault("ffmpeg", {})
            templates["h264"] = self.h264_template or H264_ENCODE
            templates[LIVE_H264] = self.live_template or LIVE_TEMPLATE
        return config

    def config_text(self, wants: set[str]) -> str:
        return yaml.safe_dump(self.config(wants), sort_keys=False)

    def whep_url(self, stream: str) -> str:
        return f"http://127.0.0.1:{self.api_port}/api/webrtc?src={stream}"

    def supports_preload_api(self) -> bool:
        return True

    def preload(self, stream: str, query: str = "") -> int:
        url = f"http://127.0.0.1:{self.api_port}/api/preload?src={stream}" + (f"&{query}" if query else "")
        # openIVR waits 90 s (HOLD_TIMEOUT_SECONDS): go2rtc may dial every source first.
        return self.request("PUT", url, timeout=90)[0]


ADAPTER = Go2rtc
