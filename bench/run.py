"""Interleaved runner: scenario x contender x repetitions, one raw JSON per run.

Usage (from the repository root, Python 3.12 with psutil and PyYAML):

    python bench/run.py --scenarios B01 --servers go2rtc,oxrtc --reps 3
    python bench/run.py --scenarios B01 --servers go2rtc --reps 1 --duration 20   # smoke

Order: repetition-major, contenders interleaved inside each repetition
(A B A B ..., or A B B A ... with --order abba), never all of A then all of B.
Each run starts from nothing - camera server, publishers, server under test,
clients - and stops all of it by handle before the next. The machine's load is
read just before each run and stored with it. A contender whose binary is
absent is skipped (and the skip recorded); so is a scenario whose status is
not `ready` (needs-linux off Linux, not-implemented always).

Output: bench/results/<session>/<run>.json (git-ignored), plus each run's
working files (configs, logs, probe output) in bench/results/<session>/<run>/.
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import platform
import sys
import threading
import time
from datetime import UTC, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import scenario as scenarios  # noqa: E402
from cams import media  # noqa: E402
from cams.publisher import CameraServer, Publisher, calibrate, stream_names  # noqa: E402
from clients.browser import BrowserProbe, Viewer, analyze_viewer  # noqa: E402
from clients.preload import Preload  # noqa: E402
from clients.rtsp_reader import RtspReader  # noqa: E402
from common import RESULTS, binary, binary_version, machine_facts  # noqa: E402
from sample import TreeSampler, background_load, summarize  # noqa: E402
from servers.base import Camera  # noqa: E402

CONTENDERS = ("go2rtc", "oxrtc", "mediamtx", "rust2rtc_gpl")


def adapter(name: str):
    if name not in CONTENDERS:
        raise SystemExit(f"unknown contender {name!r}; one of {', '.join(CONTENDERS)}")
    return importlib.import_module(f"servers.{name}").ADAPTER


def schedule(names: list[str], reps: int, order: str) -> list[tuple[int, str]]:
    """(repetition, contender) in run order: interleaved, never batched."""
    runs = []
    for rep in range(reps):
        seq = list(names)
        if order == "abba" and rep % 2 == 1:
            seq.reverse()
        runs += [(rep, name) for name in seq]
    return runs


def redact_config(text: str, server) -> str:
    return text.replace(server.password, "***") if server.password else text


class Run:
    """One scenario x contender x repetition."""

    def __init__(self, data: dict, server_cls, rep: int, session_dir: Path, duration: float | None):
        self.data = data
        self.server_cls = server_cls
        self.rep = rep
        self.duration = float(duration or data["duration_s"])
        stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
        self.run_id = f"{data['id']}_{server_cls.name}_r{rep + 1}_{stamp}"
        self.dir = session_dir / self.run_id
        self.json_path = session_dir / f"{self.run_id}.json"
        self.profile = scenarios.profile(data)
        self.count = data["cameras"]["count"]
        self.publishers: dict[int, Publisher] = {}
        self.events: list[dict] = []
        self._e0: dict[str, float | None] = {}
        #: Set when the run ends: the fault thread must not restart a publisher after it.
        self._halt = threading.Event()
        self._lock = threading.Lock()

    def execute(self) -> dict:
        record: dict = {
            "schema": 1, "run_id": self.run_id, "scenario": self.data, "repetition": self.rep + 1,
            "duration_s": self.duration, "started_at": datetime.now(UTC).isoformat(),
            "machine": machine_facts(),
            "tools": {
                "ffmpeg": binary_version(binary("ffmpeg"), ["-hide_banner", "-version"]),
                "camera_server": binary_version(binary("go2rtc"), ["-version"]),
                "node": binary_version(binary("node"), ["--version"]),
            },
            "profile": self.profile.__dict__,
        }
        record["background"] = background_load()
        media_path = media.build(self.profile)
        cam_server = CameraServer(self.dir / "cams", self.count)
        server = None
        sampler = probe = None
        readers: list[RtspReader] = []
        preloads: list[Preload] = []
        faults: threading.Thread | None = None
        try:
            cam_server.start()
            for i in range(1, self.count + 1):
                self.publishers[i] = Publisher(i, media_path, self.profile, cam_server, self.dir / "cams").start()
            for pub in self.publishers.values():
                pub.wait_ready()
            record["calibration"] = self._calibrate(cam_server)
            self._e0 = {cam: c.get("E0_ms") for cam, c in record["calibration"].items()}
            cameras = [Camera(i, *(cam_server.url(n) for n in stream_names(i))) for i in range(1, self.count + 1)]
            server = self.server_cls(self.dir / "server", cameras)
            wants = scenarios.wants(self.data)
            server.start(wants)
            record["server"] = server.describe()
            record["server_config"] = redact_config(server.config_text(wants), server)
            for group in self.data.get("preloads") or []:
                for cam in scenarios.camera_list(group, self.count):
                    preloads.append(Preload(f"cam{cam}", server.stream(cam, sub=bool(group.get("sub"))),
                                            self.dir / "clients").start(server))
            sampler = TreeSampler(server.pid).start()
            t0 = time.time()
            for group in self.data.get("rtsp") or []:
                for cam in scenarios.camera_list(group, self.count):
                    for k in range(int(group.get("per_camera", 1))):
                        url = server.rtsp_url(server.stream(cam, sub=bool(group.get("sub"))))
                        readers.append(RtspReader(f"cam{cam}-{k + 1}", url, self.duration, self.dir / "clients",
                                                  frames=bool(group.get("frames"))).start())
            viewers = self._viewers(server)
            if viewers:
                origin = server.whep_url("x").split("/", 3)
                probe = BrowserProbe(self.dir / "clients", "/".join(origin[:3]), viewers, self.duration,
                                     channel=os.environ.get("OXRTC_BROWSER_CHANNEL", "msedge")).start()
            faults = threading.Thread(target=self._faults, args=(t0, cam_server, media_path), daemon=True)
            faults.start()
            raw = probe.wait() if probe else {}
            if not probe:
                time.sleep(max(0.0, self.duration - (time.time() - t0)))
            self._halt.set()
            faults.join(timeout=30)
            record["readers"] = [r.result() for r in readers]
            record["preloads"] = [p.stop() for p in preloads]
            record["samples"] = sampler.stop()
            sampler = None
            record["summary"] = summarize(record["samples"])
            record["server_alive_at_end"] = server.child.alive() if server.child else False
            record["faults"] = self.events
            record["browser"] = self._browser(raw)
            record["status"] = "ok"
            problems = []
            if probe and (raw.get("error") or len(raw.get("viewers") or []) != len(viewers)):
                problems.append(f"browser probe: {raw.get('error') or 'viewers missing'}")
            if probe and not any(v.get("frames_presented") for v in record["browser"]["viewers"]):
                problems.append("no viewer presented a frame")
            if not record["server_alive_at_end"]:
                problems.append("server under test exited during the run")
            if problems:
                record["status"] = "error"
                record["error"] = "; ".join(problems)[:2000]
        except Exception as err:  # noqa: BLE001 - a failed run is recorded, then the next one runs
            record["status"] = "error"
            record["error"] = f"{type(err).__name__}: {err}"[:2000]
        finally:
            self._halt.set()
            if faults is not None:
                faults.join(timeout=30)
            if sampler is not None:
                record.setdefault("samples", sampler.stop())
            if probe is not None:
                probe.stop()
            for r in readers:
                if r.child:
                    r.child.stop()
            for p in preloads:
                p.stop()
            if server is not None:
                server.stop()
            with self._lock:
                for pub in self.publishers.values():
                    pub.stop()
            cam_server.stop()
            record["ended_at"] = datetime.now(UTC).isoformat()
        self.json_path.parent.mkdir(parents=True, exist_ok=True)
        self.json_path.write_text(json.dumps(record), encoding="utf-8")
        return record

    def _decoded_cameras(self) -> list[int]:
        cams: set[int] = set()
        for group in self.data.get("webrtc") or []:
            if int(group.get("decode_first", 0)) > 0:
                cams.update(scenarios.camera_list(group, self.count))
        return sorted(cams)

    def _calibrate(self, cam_server: CameraServer) -> dict:
        out: dict = {}
        threads = []
        for cam in self._decoded_cameras()[:8]:  # bounded: latency is read on a few cameras
            main, sub = stream_names(cam)

            def job(cam=cam, sub=sub):
                out[str(cam)] = calibrate(cam_server.url(sub), self.profile, self.dir / "cams", 8.0)

            t = threading.Thread(target=job, daemon=True)
            t.start()
            threads.append(t)
        for t in threads:
            t.join(timeout=30)
        return out

    def _viewers(self, server) -> list[Viewer]:
        viewers = []
        for group in self.data.get("webrtc") or []:
            want = group.get("want", "copy")
            decode_left = int(group.get("decode_first", 0))
            for cam in scenarios.camera_list(group, self.count):
                for k in range(int(group.get("per_camera", 1))):
                    stream = server.stream(cam, want, sub=bool(group.get("sub")))
                    viewers.append(Viewer(
                        label=f"cam{cam}-{want}-{k + 1}", url=server.whep_url(stream),
                        headers=server.auth_header(), reference_width=self.profile.width,
                        decode=decode_left > 0, cycles=int(group.get("cycles", 0)),
                        hold_after_first_frame_ms=int(group.get("hold_ms", 1000)),
                        gap_ms=int(group.get("gap_ms", 0)),
                        spread_ms=int(group.get("spread_ms", 0)),
                    ))
                    decode_left -= 1
        return viewers

    def _faults(self, t0: float, cam_server: CameraServer, media_path: Path) -> None:
        """Camera outages, inside the measured window only, stopped as soon as the run ends.

        A fault that would end after the window (a smoke run's --duration) is
        recorded as skipped, not played: a publisher restarted after the
        run's teardown would outlive it. Every wait is on `_halt`, so the
        teardown never races a restart.
        """
        for fault in sorted(self.data.get("faults") or [], key=lambda f: f["at_s"]):
            cams = scenarios.camera_list(fault, self.count)
            if fault["at_s"] + fault["down_s"] >= self.duration:
                self.events.append({"cameras": cams, "skipped": "ends after the measured window",
                                    "at_s": fault["at_s"], "down_s": fault["down_s"]})
                continue
            if self._halt.wait(max(0.0, t0 + fault["at_s"] - time.time())):
                return
            down = time.time() * 1000
            with self._lock:
                for cam in cams:
                    self.publishers[cam].stop()
            if self._halt.wait(fault["down_s"]):
                return
            with self._lock:
                if self._halt.is_set():
                    return
                for cam in cams:
                    self.publishers[cam] = Publisher(cam, media_path, self.profile, cam_server,
                                                     self.dir / "cams").start()
            self.events.append({"cameras": cams, "down_wall_ms": down, "up_wall_ms": time.time() * 1000,
                                "down_s": fault["down_s"]})

    def _browser(self, raw: dict) -> dict:
        viewers = []
        for v in raw.get("viewers") or []:
            cam = (v.get("label") or "cam0").split("-")[0][3:]
            e0 = self._e0.get(cam)
            figures = analyze_viewer(v, e0, self.profile.frame_ms)
            figures["recovery_ms"] = self._recovery(v)
            viewers.append(figures)
        return {
            "error": raw.get("error"),
            "channel": raw.get("channel"),
            "ua": raw.get("ua"),
            "clock_skew_ms": [raw.get("clockSkewStartMs"), raw.get("clockSkewEndMs")],
            "console_errors": raw.get("consoleErrors"),
            "viewers": viewers,
            "raw": raw,
        }

    def _recovery(self, viewer: dict) -> list[float | None]:
        frames = viewer.get("frames") or []
        out = []
        for event in self.events:
            if "up_wall_ms" not in event:
                continue
            nxt = next((f[0] for f in frames if f[0] > event["up_wall_ms"]), None)
            out.append(None if nxt is None else nxt - event["up_wall_ms"])
        return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--scenarios", default="B01", help="comma-separated IDs, or 'all'")
    parser.add_argument("--servers", default="go2rtc", help=f"comma-separated, from {', '.join(CONTENDERS)}")
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--order", choices=("abab", "abba"), default="abab")
    parser.add_argument("--duration", type=float, help="override every scenario's duration_s (smoke runs)")
    parser.add_argument("--session", help="session folder name under bench/results/")
    ns = parser.parse_args(argv)
    if ns.reps < 1 or ns.reps > 100:
        raise SystemExit("--reps must be 1..100")
    ids = scenarios.all_ids() if ns.scenarios == "all" else ns.scenarios.split(",")
    session = ns.session or datetime.now(UTC).strftime("session_%Y%m%dT%H%M%SZ")
    session_dir = RESULTS / session
    skipped = []
    names = []
    for name in ns.servers.split(","):
        cls = adapter(name)
        if not cls.available():
            skipped.append({"server": name, "reason": f"binary {cls.binary_name} not found"})
            print(f"skip {name}: binary not found")
        else:
            names.append(name)
            if not cls.tested:
                print(f"warning: the {name} adapter is untested ({cls.provenance})")
    for sid in ids:
        data = scenarios.load(sid)
        ready = data["status"] == "ready" or (data["status"] == "needs-linux" and platform.system() == "Linux")
        if not ready:
            skipped.append({"scenario": sid, "reason": data.get("reason", data["status"]).strip()})
            print(f"skip {sid}: {data['status']}")
            continue
        for rep, name in schedule(names, ns.reps, ns.order):
            run = Run(data, adapter(name), rep, session_dir, ns.duration)
            print(f"run {run.run_id} ...", flush=True)
            record = run.execute()
            print(f"  {record['status']} {record.get('error', '')}".rstrip(), flush=True)
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "session.json").write_text(json.dumps({"skipped": skipped, "argv": sys.argv[1:]}, indent=2),
                                              encoding="utf-8")
    print(f"results in {session_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
