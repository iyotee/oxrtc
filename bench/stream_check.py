"""One MediaStream per session, as a browser sees it (roadmap W1).

Usage (from the repository root, Python 3.12 with psutil and PyYAML):

    python bench/stream_check.py --servers go2rtc,oxrtc,mediamtx
    python bench/stream_check.py --servers oxrtc --offer video,audio,audio
    python bench/stream_check.py --servers oxrtc --camera-audio none
    python bench/stream_check.py --servers oxrtc --player own

For each server: one synthetic camera (H.264 and PCMA, the bench's own), the
server started on free ports as for a bench run, and ONE headless browser
whose player does `video.srcObject = event.streams[0]` at every track event
(clients/stream_probe.mjs). The check passes when every track event names the
same first stream and the <video> shows a picture (videoWidth > 0).

Printed and stored (bench/results/<session>/stream_<server>.json): the track
events, the picture, the lines of the answer that name streams and tracks,
how long the browser held each presented picture (from its last packet to its
display), and the browser's jitter buffers (which tell whether it synchronises
the picture with the sound: it does so only for tracks of one stream).

Exit status 1 when a server named by --require (default: oxrtc) fails; the
others are references, reported and never required.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from datetime import UTC, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from cams import media  # noqa: E402
from cams.media import Profile  # noqa: E402
from cams.publisher import CameraServer, Publisher, stream_names  # noqa: E402
from common import RESULTS, Child, binary  # noqa: E402
from servers.base import Camera  # noqa: E402

PROBE = Path(__file__).resolve().parent / "clients" / "stream_probe.mjs"
KINDS = ("video", "audio")


def first_streams(result: dict) -> list[str | None]:
    """The id of `event.streams[0]` at each track event (None: the event had no stream)."""
    return [(t.get("streams") or [None])[0] for t in result.get("tracks") or []]


def verdict(result: dict, offered: list[str]) -> list[str]:
    """What is wrong with a probe's result; empty when the player showed one stream."""
    problems = []
    if result.get("error"):
        problems.append(f"probe: {result['error']}")
    problems += [str(e) for e in result.get("errors") or []]
    firsts = first_streams(result)
    if not firsts:
        problems.append("no track event")
    elif None in firsts:
        problems.append("a track event carried no stream")
    elif len(set(firsts)) != 1:
        problems.append(f"{len(set(firsts))} streams for {len(firsts)} tracks: event.streams[0] changes")
    video = result.get("video") or {}
    if "video" in offered and not video.get("videoWidth"):
        problems.append("no picture: videoWidth is 0 with event.streams[0] as srcObject")
    return problems


def buffer_ms(result: dict, kind: str, key: str = "jitterBufferDelay") -> float | None:
    """A jitter-buffer figure of `kind` over the second half of the probe, in ms per emitted unit."""
    def row(stats):
        return next((s for s in stats or [] if s.get("kind") == kind), None)

    half, end = row(result.get("statsHalf")), row(result.get("statsEnd"))
    if not half or not end or key not in half or key not in end:
        return None
    emitted = end.get("jitterBufferEmittedCount", 0) - half.get("jitterBufferEmittedCount", 0)
    if emitted <= 0:
        return None
    return 1000 * (end[key] - half[key]) / emitted


def picture_hold(result: dict, start_ms: float = 0, end_ms: float | None = None) -> dict | None:
    """How long the browser held each presented picture between `start_ms` and `end_ms` after the answer.

    `frames` of the probe: [ms since the answer, ms held, RTP time] per presented
    frame, "held" being the time from the frame's last packet to its display
    (jitter buffer, decoder, and the wait for the sound when the picture and
    the sound are one stream). Returns the count, the mean and the 95th
    percentile of the hold, and the longest wait between two displays beyond
    what their RTP times ask for (`longest_stall_ms`: a picture that froze).
    None when no frame fell in the window.
    """
    frames = [f for f in result.get("frames") or []
              if f[0] >= start_ms and (end_ms is None or f[0] <= end_ms)]
    if not frames:
        return None
    held = sorted(f[1] for f in frames)
    stall = 0.0
    for (at0, _, rtp0), (at1, _, rtp1) in zip(frames, frames[1:]):
        step = ((rtp1 - rtp0) % (1 << 32)) / 90  # the video clock is 90 kHz
        if step < 10_000:  # a wrap or a reordering is not a step
            stall = max(stall, (at1 - at0) - step)
    return {"frames": len(frames), "mean_ms": sum(held) / len(held),
            "p95_ms": held[min(len(held) - 1, int(0.95 * len(held)))], "longest_stall_ms": stall}


def describe(name: str, result: dict, offered: list[str]) -> list[str]:
    """The lines printed for one server."""
    out = [f"== {name}"]
    for track in result.get("tracks") or []:
        out.append(f"   track event: {track.get('kind')}, event.streams = {track.get('streams')}")
    video = result.get("video") or {}
    shown = video.get("srcObject") or {}
    out.append(f"   <video>: videoWidth {video.get('videoWidth')}, frames presented {video.get('framesPresented')},"
               f" srcObject tracks {[t.get('kind') for t in shown.get('tracks') or []]}")
    hold = picture_hold(result, 1000 * (result.get("seconds") or 0) / 2)
    if hold:
        out.append(f"   picture held by the browser (second half): mean {hold['mean_ms']:.1f} ms,"
                   f" P95 {hold['p95_ms']:.1f} ms over {hold['frames']} frames,"
                   f" longest stall {hold['longest_stall_ms']:.0f} ms")
    for kind in KINDS:
        delay, target = buffer_ms(result, kind), buffer_ms(result, kind, "jitterBufferTargetDelay")
        end = next((s for s in result.get("statsEnd") or [] if s.get("kind") == kind), None)
        if end:
            fmt = lambda v: "n/a" if v is None else f"{v:.1f} ms"  # noqa: E731
            jitter = end.get("jitter")
            out.append(f"   {kind}: jitter buffer {fmt(delay)} (target {fmt(target)}),"
                       f" arrival jitter {fmt(None if jitter is None else 1000 * jitter)},"
                       f" {end.get('packetsReceived')} packets")
    for line in result.get("answer") or []:
        if line.startswith(("a=msid", "a=ssrc")) and "cname" not in line or line.startswith("m="):
            out.append(f"   {line}")
    problems = verdict(result, offered)
    out.append("   PASS: one stream, a picture" if not problems else "   FAIL: " + "; ".join(problems))
    return out


def run_probe(workdir: Path, url: str, headers: dict, offered: list[str], seconds: float,
              player: str = "first") -> dict:
    node = binary("node")
    if node is None:
        raise FileNotFoundError("node not found (set OXRTC_NODE)")
    workdir.mkdir(parents=True, exist_ok=True)
    job_path, out_path = workdir / "stream-job.json", workdir / "stream-out.json"
    job = {"origin": "/".join(url.split("/", 3)[:3]), "url": url, "headers": headers, "seconds": seconds,
           "transceivers": offered, "player": player,
           "channel": os.environ.get("OXRTC_BROWSER_CHANNEL", "msedge")}
    job_path.write_text(json.dumps(job), encoding="utf-8")
    child = Child("stream-probe", [node, str(PROBE), str(job_path), str(out_path)], workdir / "browser.log").start()
    deadline = time.monotonic() + seconds + 60
    try:
        while child.alive() and time.monotonic() < deadline:
            time.sleep(0.5)
    finally:
        child.stop()
    try:
        return json.loads(out_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"error": f"probe wrote no output: {child.tail()[-400:]}"}


def check(name: str, offered: list[str], seconds: float, workdir: Path, player: str = "first",
          audio: str = "pcma") -> dict:
    """Starts a camera and server `name`, runs the probe once, stops everything."""
    import importlib

    server_cls = importlib.import_module(f"servers.{name}").ADAPTER
    if not server_cls.available():
        return {"error": f"binary {server_cls.binary_name} not found", "skipped": True}
    profile = Profile(audio=audio)
    media_path = media.build(profile)
    cam_server = CameraServer(workdir / "cams", 1)
    publisher = server = None
    try:
        cam_server.start()
        publisher = Publisher(1, media_path, profile, cam_server, workdir / "cams").start()
        publisher.wait_ready()
        camera = Camera(1, *(cam_server.url(n) for n in stream_names(1)))
        server = server_cls(workdir / "server", [camera])
        server.start({"copy"})
        url = server.whep_url(server.stream(1, "copy"))
        result = run_probe(workdir / "clients", url, server.auth_header(), offered, seconds, player)
        result["seconds"] = seconds
        result["server"] = {"name": name, "version": server.version()}
        return result
    finally:
        if server is not None:
            server.stop()
        if publisher is not None:
            publisher.stop()
        cam_server.stop()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--servers", default="oxrtc", help="comma-separated: go2rtc, oxrtc, mediamtx")
    parser.add_argument("--require", default="oxrtc", help="comma-separated servers that must pass")
    parser.add_argument("--offer", default="video,audio", help="the recvonly transceivers offered, in order")
    parser.add_argument("--seconds", type=float, default=12.0, help="how long the player runs")
    parser.add_argument("--player", choices=("first", "own"), default="first",
                        help="first: srcObject = event.streams[0] at every track event (the check);"
                             " own: a stream of the page's own that every track is added to")
    parser.add_argument("--camera-audio", choices=("pcma", "none"), default="pcma",
                        help="the camera's sound: none is a camera with no microphone")
    parser.add_argument("--session", help="session folder name under bench/results/")
    ns = parser.parse_args(argv)
    offered = ns.offer.split(",")
    if not offered or any(kind not in KINDS for kind in offered) or len(offered) > 8:
        raise SystemExit(f"--offer is one to eight of {', '.join(KINDS)}")
    session = RESULTS / (ns.session or datetime.now(UTC).strftime("stream_%Y%m%dT%H%M%SZ"))
    required, failed = set(ns.require.split(",")), []
    for name in ns.servers.split(","):
        result = check(name, offered, ns.seconds, session / name, ns.player, ns.camera_audio)
        session.mkdir(parents=True, exist_ok=True)
        (session / f"stream_{name}.json").write_text(json.dumps(result, indent=1), encoding="utf-8")
        if result.get("skipped"):
            print(f"== {name}\n   skipped: {result['error']}")
            continue
        print("\n".join(describe(name, result, offered)), flush=True)
        if name in required and verdict(result, offered):
            failed.append(name)
    print(f"results in {session}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
