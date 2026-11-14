"""The camera side: a dedicated go2rtc acting as the cameras' RTSP server, and
ONE FFmpeg process per camera publishing its main and substream together.

The camera server is a go2rtc of its own, on its own free ports, distinct from
the server under test (even when that one is go2rtc too): every contender then
reads exactly the same RTSP from the same process. Each camera's FFmpeg reads
its media file (cams/media.py: main and substream in one file, frame for frame)
in real time, looping, and pushes both streams with RTSP ANNOUNCE/RECORD over
TCP - one demuxer for both, so the two streams cannot drift apart.

`calibrate()` reads the frame code of a stream straight off the camera server
with a single-threaded decoder and returns the emitter's clock E0 (framecode).
"""

from __future__ import annotations

import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import framecode  # noqa: E402
from cams.media import Profile  # noqa: E402
from common import Child, binary, contain, free_port, free_udp_port, wait_until  # noqa: E402


def stream_names(index: int) -> tuple[str, str]:
    return f"cam{index}_main", f"cam{index}_sub"


@dataclass
class CameraServer:
    """go2rtc serving the synthetic cameras' publications."""

    workdir: Path
    cameras: int
    api_port: int = field(default_factory=free_port)
    rtsp_port: int = field(default_factory=free_port)
    webrtc_port: int = field(default_factory=free_udp_port)
    child: Child | None = None

    def config(self) -> dict:
        streams = {}
        for i in range(1, self.cameras + 1):
            for name in stream_names(i):
                streams[name] = None  # a publication target: FFmpeg pushes to it
        return {
            "api": {"listen": f"127.0.0.1:{self.api_port}"},
            "rtsp": {"listen": f"127.0.0.1:{self.rtsp_port}"},
            "webrtc": {"listen": f"127.0.0.1:{self.webrtc_port}"},
            "srtp": {"listen": ""},
            "log": {"level": "warn"},
            "streams": streams,
        }

    def url(self, stream: str) -> str:
        return f"rtsp://127.0.0.1:{self.rtsp_port}/{stream}"

    def start(self) -> "CameraServer":
        go2rtc = binary("go2rtc")
        if go2rtc is None:
            raise FileNotFoundError("go2rtc not found (set OXRTC_GO2RTC)")
        self.workdir.mkdir(parents=True, exist_ok=True)
        path = self.workdir / "camera-server.yaml"
        path.write_text(yaml.safe_dump(self.config(), sort_keys=False), encoding="utf-8")
        self.child = Child("camera-server", [go2rtc, "-config", str(path)],
                           self.workdir / "camera-server.log", cwd=self.workdir).start()
        if not wait_until(lambda: _http_ok(f"http://127.0.0.1:{self.api_port}/api"), 15):
            raise RuntimeError(f"camera server did not come up:\n{self.child.tail()}")
        return self

    def producers_ready(self, stream: str) -> bool:
        import json
        import urllib.request

        url = f"http://127.0.0.1:{self.api_port}/api/streams?src={stream}"
        with urllib.request.urlopen(url, timeout=2) as resp:  # noqa: S310 - loopback
            info = json.loads(resp.read().decode("utf-8"))
        return any(p.get("medias") for p in info.get("producers") or [])

    def stop(self) -> None:
        if self.child is not None:
            self.child.stop()


@dataclass
class Publisher:
    """One FFmpeg pushing camera `index`'s main and substream to the camera server."""

    index: int
    media: Path
    profile: Profile
    server: CameraServer
    workdir: Path
    child: Child | None = None
    started_wall_ms: float | None = None

    def argv(self, ffmpeg: str) -> list[str]:
        main, sub = stream_names(self.index)
        audio = ["-map", "0:a:0?"] if self.profile.audio != "none" else []
        # Parameter sets in-band before every keyframe, as a camera sends them
        # (Matroska keeps them in the codec header only).
        annexb = "h264_mp4toannexb" if self.profile.codec == "h264" else "hevc_mp4toannexb"
        out = ["-c", "copy", "-bsf:v", annexb, "-f", "rtsp", "-rtsp_transport", "tcp"]
        return [
            ffmpeg, "-hide_banner", "-loglevel", "warning", "-nostats",
            "-readrate", "1", "-readrate_initial_burst", "0", "-readrate_catchup", "1",
            "-stream_loop", "-1", "-i", str(self.media),
            "-map", "0:v:0", *audio, *out, self.server.url(main),
            "-map", "0:v:1", *audio, *out, self.server.url(sub),
        ]

    def start(self) -> "Publisher":
        ffmpeg = binary("ffmpeg")
        if ffmpeg is None:
            raise FileNotFoundError("ffmpeg not found (set OXRTC_FFMPEG)")
        self.child = Child(f"cam{self.index}", self.argv(ffmpeg),
                           self.workdir / f"cam{self.index}.log").start()
        self.started_wall_ms = time.time() * 1000
        return self

    def wait_ready(self, timeout: float = 20) -> None:
        main, sub = stream_names(self.index)
        ok = wait_until(lambda: self.server.producers_ready(main) and self.server.producers_ready(sub), timeout)
        if not ok:
            tail = self.child.tail() if self.child else ""
            raise RuntimeError(f"camera {self.index} did not publish:\n{tail}")

    def stop(self) -> None:
        if self.child is not None:
            self.child.stop()


def _http_ok(url: str) -> bool:
    import urllib.request

    with urllib.request.urlopen(url, timeout=2) as resp:  # noqa: S310 - loopback
        return resp.status == 200


def read_codes(url: str, reference_width: int, seconds: float, workdir: Path) -> list[tuple[float, int]]:
    """(arrival wall ms, code) of every frame of `url` for `seconds`, single-threaded decode."""
    ffmpeg = binary("ffmpeg")
    if ffmpeg is None:
        raise FileNotFoundError("ffmpeg not found (set OXRTC_FFMPEG)")
    argv = [
        ffmpeg, "-hide_banner", "-loglevel", "error", "-nostats",
        "-rtsp_transport", "tcp", "-fflags", "nobuffer", "-flags", "low_delay",
        "-threads", "1", "-i", url, "-an", "-threads", "1",
        "-vf", framecode.thumbnail_filter(reference_width),
        "-fps_mode", "passthrough", "-flush_packets", "1",
        "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1",
    ]
    size = framecode.BITS * framecode.CELL * framecode.CELL
    width, height = framecode.BITS * framecode.CELL, framecode.CELL
    proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,  # noqa: S603
                            stderr=subprocess.DEVNULL)
    job = contain(proc)  # dies with the runner, even if the runner is killed
    samples: list[tuple[float, int]] = []
    timer = threading.Timer(seconds, proc.kill)
    timer.start()
    try:
        assert proc.stdout is not None
        while True:
            chunk = proc.stdout.read(size)
            if len(chunk) < size:
                break
            samples.append((time.time() * 1000, framecode.decode_gray(chunk, width, height)))
            if len(samples) > 100_000:
                break
    finally:
        timer.cancel()
        proc.kill()
        if job is not None:
            job.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
    return samples


#: Frames the calibration reader holds each picture before handing it over.
#: FFmpeg's H.264/H.265 parser closes an access unit only when the next one
#: starts, so the reader sees frame n when frame n+1 arrives - one frame
#: interval late. Measured on 2026-11-14 (go2rtc, Edge, loopback, no audio):
#: before the correction the browser "saw" frames 28 ms before their emission
#: at 25 fps and 56 ms before at 100/7 fps - the offset scales with the frame
#: interval (40 vs 70 ms), which a fixed delay would not.
READER_HOLD_FRAMES = 1


def calibrate(url: str, profile: Profile, workdir: Path, seconds: float = 6.0) -> dict:
    """The emitter's clock for one camera, read off the camera server."""
    raw = read_codes(url, profile.width, seconds, workdir)
    flags = framecode.consistent([c for _, c in raw])
    good = [s for s, ok in zip(raw, flags) if ok][15:]  # past the join burst
    e0 = framecode.calibrate(good, profile.frame_ms)
    if e0 is not None:
        e0 -= READER_HOLD_FRAMES * profile.frame_ms
    excess = []
    if e0 is not None:
        excess = sorted(framecode.latency_ms(w, c, e0, profile.frame_ms) for w, c in good)
    return {
        "url_stream": url.rsplit("/", 1)[-1],
        "frames_read": len(raw),
        "frames_used": len(good),
        "E0_ms": e0,
        "reader_hold_frames": READER_HOLD_FRAMES,
        "frame_ms": profile.frame_ms,
        "reader_excess_ms_mean": (sum(excess) / len(excess)) if excess else None,
        "reader_excess_ms_p95": framecode.percentile(excess, 95) if excess else None,
    }
