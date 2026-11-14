"""The synthetic cameras' media: one file per camera profile, main + substream inside.

`build(profile)` encodes, ONCE and cached under bench/media/ (git-ignored), a
Matroska file with two video streams cut from the same `testsrc2` picture -
main and substream, frame for frame - and an audio track:

- the 12-bit frame code (framecode.py) burnt into the top-left corner of the
  main picture, scaled with it into the substream;
- a white flash beside the code for the first 100 ms of every second, and a
  1 kHz beep at the same instant on the audio track (A/V sync checks);
- exactly 4096 frames, so that when the publisher loops the file the code runs
  on without a jump (4096 = the code's own wrap).

Encoding is done here, before any run, so a run costs the camera side only a
remux (`-c copy`): the publisher does not compete with the server under test
for the CPU. Profiles differ by codec, size, fps and GOP only.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from dataclasses import asdict, dataclass
from fractions import Fraction
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import framecode  # noqa: E402
from common import BENCH, binary  # noqa: E402

MEDIA = BENCH / "media"
FRAMES = framecode.MODULO
#: Bumped whenever build_argv() changes what a file contains: cached files are keyed by it.
MEDIA_FORMAT = 2


@dataclass(frozen=True)
class Profile:
    codec: str = "h264"  # h264 | h265
    width: int = 1920
    height: int = 1080
    fps: str = "25"  # a rational, e.g. "100/7" (the owner's cameras)
    gop: int = 25  # frames between keyframes
    sub_width: int = 512
    sub_height: int = 288
    bitrate: str = "4M"
    sub_bitrate: str = "300k"
    audio: str = "pcma"  # pcma (PCMA/8000) | none
    encoder: str = "cpu"  # cpu (libx264/libx265) | nvenc

    @property
    def frame_ms(self) -> float:
        return float(1000 / Fraction(self.fps))

    @property
    def duration_s(self) -> Fraction:
        return FRAMES / Fraction(self.fps)

    def key(self) -> str:
        blob = json.dumps({**asdict(self), "format": MEDIA_FORMAT}, sort_keys=True).encode()
        return f"{self.codec}_{self.width}x{self.height}_g{self.gop}_" + hashlib.sha256(blob).hexdigest()[:10]

    def path(self) -> Path:
        return MEDIA / f"{self.key()}.mkv"


def _video_codec_args(profile: Profile, index: int, bitrate: str) -> list[str]:
    s = f":v:{index}"
    gop = str(profile.gop)
    if profile.encoder == "nvenc":
        name = "h264_nvenc" if profile.codec == "h264" else "hevc_nvenc"
        return [f"-c{s}", name, f"-preset{s}", "p4", f"-b{s}", bitrate, f"-g{s}", gop,
                f"-bf{s}", "0", f"-forced-idr{s}", "1"]
    if profile.codec == "h264":
        return [f"-c{s}", "libx264", f"-preset{s}", "veryfast", f"-profile{s}", "high",
                f"-b{s}", bitrate, f"-g{s}", gop, f"-keyint_min{s}", gop,
                f"-sc_threshold{s}", "0", f"-bf{s}", "0"]
    if profile.codec == "h265":
        params = f"keyint={gop}:min-keyint={gop}:scenecut=0:bframes=0:log-level=error"
        return [f"-c{s}", "libx265", f"-preset{s}", "veryfast", f"-b{s}", bitrate,
                f"-x265-params{s}", params, f"-tag{s}", "hvc1"]
    raise ValueError(f"unknown codec {profile.codec!r}")


def build_argv(profile: Profile, ffmpeg: str, out: Path) -> list[str]:
    """The single FFmpeg invocation that encodes the profile's file."""
    fps = profile.fps
    duration = f"{float(profile.duration_s):.6f}"
    flash = (
        f"drawbox=x={(framecode.BITS + 1) * framecode.SQUARE}:y=0:w={2 * framecode.SQUARE}"
        f":h={2 * framecode.SQUARE}:color=white:t=fill:enable='lt(mod(t\\,1)\\,0.1)'"
    )
    graph = (
        f"[0:v]trim=end_frame={FRAMES},format=yuv420p,{framecode.ffmpeg_filter()},{flash},"
        f"split=2[main][s0];[s0]scale={profile.sub_width}:{profile.sub_height}[sub]"
    )
    argv = [
        ffmpeg, "-hide_banner", "-loglevel", "error", "-nostats", "-y",
        "-f", "lavfi", "-i", f"testsrc2=size={profile.width}x{profile.height}:rate={fps}",
    ]
    maps = ["-map", "[main]", "-map", "[sub]"]
    if profile.audio == "pcma":
        argv += ["-f", "lavfi", "-i",
                 "aevalsrc='if(lt(mod(t\\,1)\\,0.1)\\,0.5*sin(2*PI*1000*t)\\,0)':s=8000:c=mono"]
        # 20 ms packets, as a camera sends them: 128 ms ones would make the
        # publisher release video in bursts at the audio cadence.
        graph += f";[1:a]atrim=end={duration},asetnsamples=n=160:p=0[aud]"
        maps += ["-map", "[aud]"]
    argv += ["-filter_complex", graph, *maps]
    argv += _video_codec_args(profile, 0, profile.bitrate)
    argv += _video_codec_args(profile, 1, profile.sub_bitrate)
    if profile.audio == "pcma":
        argv += ["-c:a", "pcm_alaw", "-ar", "8000", "-ac", "1"]
    argv += [str(out)]
    return argv


def build(profile: Profile, force: bool = False) -> Path:
    """The profile's media file, encoded now unless already cached."""
    out = profile.path()
    if out.is_file() and not force:
        return out
    ffmpeg = binary("ffmpeg")
    if ffmpeg is None:
        raise FileNotFoundError("ffmpeg not found (set OXRTC_FFMPEG)")
    MEDIA.mkdir(parents=True, exist_ok=True)
    partial = out.with_suffix(".partial.mkv")
    subprocess.run(build_argv(profile, ffmpeg, partial), check=True, timeout=3600)  # noqa: S603
    partial.replace(out)
    (out.with_suffix(".json")).write_text(json.dumps(asdict(profile), indent=2), encoding="utf-8")
    return out


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="Build a synthetic camera's media file.")
    for name, default in asdict(Profile()).items():
        parser.add_argument(f"--{name.replace('_', '-')}", type=type(default), default=default)
    parser.add_argument("--force", action="store_true")
    ns = vars(parser.parse_args())
    force = ns.pop("force")
    print(build(Profile(**ns), force=force))
