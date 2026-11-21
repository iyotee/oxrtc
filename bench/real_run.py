"""A run on the owner's real cameras (scenarios RC1-RC4, `layout: real`).

Same shape as run.Run - server under test, preloads, RTSP readers, one headless
browser holding the viewers, a process-tree sampler - with what real cameras
change:

- no camera server and no publisher: the sources are the cameras themselves,
  read from openIVR's config (real.py) and never written anywhere;
- one contender at a time (the runner's order), and a pause after each run so
  the fragile cameras release their connection;
- no frame code: real pictures carry none, so nothing here reads latency;
- a camera that does not answer the preflight stays in the server's config, as
  in openIVR, gets no client, and is a row of its own ("unreachable"); when
  none answers (this machine's network was down) the run is an error, not a
  repetition of an idle server;
- a viewer that shows nothing is a RESULT (failed), not an error of the run;
- every text that is recorded goes through real.scrub().
"""

from __future__ import annotations

import json
import os
import sys
import time
from datetime import UTC, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import real  # noqa: E402
import scenario as scenarios  # noqa: E402
from clients.browser import BrowserProbe, Viewer, analyze_viewer  # noqa: E402
from clients.preload import Preload  # noqa: E402
from clients.rtsp_reader import RtspReader, reader_label  # noqa: E402
from common import RESULTS, binary, binary_version, machine_facts  # noqa: E402
from sample import TreeSampler, background_load, summarize  # noqa: E402
from servers.base import Camera  # noqa: E402

#: A gap between two presented frames longer than this is a freeze (RC1's definition) ...
FREEZE_MS = 1000
#: ... unless the source is slow itself: a webcam sending one picture a second
#: has a gap of 1000 ms between every two pictures. Its freeze threshold is this
#: many source intervals (read from the recorder's frame rate on that camera).
SLOW_SOURCE_INTERVALS = 2.5


def freeze_threshold_ms(source_fps: float | None) -> float:
    """1000 ms, or 2.5 source intervals when the source sends fewer than 2.5 pictures a second."""
    if not source_fps or source_fps <= 0:
        return float(FREEZE_MS)
    return max(float(FREEZE_MS), 1000.0 * SLOW_SOURCE_INTERVALS / source_fps)
#: Seconds the cameras get to release their connection between two runs.
SETTLE_S = 12


def freeze_figures(frames: list[list[float]], threshold_ms: float = FREEZE_MS, skip_s: float = 5.0) -> dict:
    """Freezes of ONE session: gaps between presented frames above `threshold_ms`.

    `frames` are `[wall_ms, code, presentedFrames]` as the probe records them.
    The first `skip_s` seconds (the connection settling) are left out. A gap that
    spans a reconnection is not told apart here: RC1 and RC4 hold one session.
    """
    times = [f[0] for f in frames]
    if len(times) < 2:
        return {"freezes_1s": None, "frozen_s": None, "longest_gap_ms": None, "fps_steady": None}
    start = times[0] + skip_s * 1000
    steady = [t for t in times if t >= start]
    if len(steady) < 2:
        return {"freezes_1s": None, "frozen_s": None, "longest_gap_ms": None, "fps_steady": None}
    gaps = [b - a for a, b in zip(steady, steady[1:])]
    long = [g for g in gaps if g > threshold_ms]
    span = (steady[-1] - steady[0]) / 1000
    return {
        "freezes_1s": len(long),
        "frozen_s": sum(long) / 1000,
        "longest_gap_ms": max(gaps),
        "fps_steady": (len(steady) - 1) / span if span > 0 else None,
    }


def stats_fps(viewer: dict, skip_s: float = 5.0) -> float | None:
    """Frames the browser DECODED per second over the steady part, from getStats() (an independent count)."""
    rows = [s for s in viewer.get("stats") or [] if s.get("kind") == "video" and s.get("framesDecoded") is not None]
    if len(rows) < 2:
        return None
    start = rows[0]["wall"] + skip_s * 1000
    steady = [r for r in rows if r["wall"] >= start]
    if len(steady) < 2:
        return None
    span = (steady[-1]["wall"] - steady[0]["wall"]) / 1000
    return (steady[-1]["framesDecoded"] - steady[0]["framesDecoded"]) / span if span > 0 else None


def merge_probes(outputs: list[dict]) -> dict:
    """The outputs of several browser processes as one probe's: every viewer,
    each browser's error named by its number, the first browser's facts."""
    if len(outputs) == 1:
        return outputs[0]
    first = outputs[0] if outputs else {}
    errors = [f"browser {k + 1}: {o['error']}" for k, o in enumerate(outputs) if o.get("error")]
    return {**first, "error": "; ".join(errors) or None,
            "viewers": [v for o in outputs for v in o.get("viewers") or []],
            "consoleErrors": [e for o in outputs for e in o.get("consoleErrors") or []][:50]}


class SetChanged(real.RealCameraError):
    """A camera of the session's first run is no longer in openIVR's configuration."""


def measured_ids(record: dict) -> list[int] | None:
    """The cameras a real run measured (configured and kept, reachable or not), or None for a failed run."""
    if record.get("status") != "ok" or not record.get("cameras"):
        return None
    return [int(name.rsplit("_", 1)[1]) for name in record["cameras"]]


class RealRun:
    """One real-camera scenario x contender x repetition."""

    def __init__(self, data: dict, server_cls, rep: int, session_dir: Path, duration: float | None,
                 pinned: list[int] | None = None):
        """`pinned`: the cameras the session's first run measured (run.py passes
        them to every later run of the scenario): a session compares the
        contenders on ONE set of cameras, so a camera the owner enables during
        it is left out and one he disables ends the session (`SetChanged`)."""
        self.data = data
        self.pinned = list(pinned) if pinned is not None else None
        self.server_cls = server_cls
        self.rep = rep
        self.duration = float(duration or data["duration_s"])
        stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
        self.run_id = f"{data['id']}_{server_cls.name}_r{rep + 1}_{stamp}"
        self.dir = session_dir / self.run_id
        self.json_path = session_dir / f"{self.run_id}.json"
        self.ids = self.pinned if self.pinned is not None else scenarios.real_ids(data)
        self.count = len(self.ids)
        self.labels = {int(k): v for k, v in ((data.get("cameras") or {}).get("labels") or {}).items()}

    def execute(self) -> dict:
        record: dict = {
            "schema": 2, "run_id": self.run_id, "scenario": self.data, "repetition": self.rep + 1,
            "duration_s": self.duration, "started_at": datetime.now(UTC).isoformat(),
            "machine": machine_facts(),
            "tools": {
                "ffmpeg": binary_version(binary("ffmpeg"), ["-hide_banner", "-version"]),
                "node": binary_version(binary("node"), ["--version"]),
            },
            "real": True,
        }
        cams: list[real.RealCamera] = []
        server = sampler = None
        probes: list[BrowserProbe] = []
        readers: list[RtspReader] = []
        preloads: list[Preload] = []
        try:
            config = real.load_config()
            absent: list[int] = []
            cams = real.load_cameras(self.ids, config, absent=absent)
            # Removed or disabled by the owner since the scenario was written.
            record["cameras_absent"] = [f"camera_{cid}" for cid in absent]
            if self.pinned is not None and absent:
                record["camera_set_changed"] = True
                raise SetChanged(f"the session's cameras changed: {', '.join(record['cameras_absent'])} "
                                 "disabled or removed in openIVR since its first run; a session compares "
                                 "the contenders on one set of cameras, start a new one")
            record["background"] = background_load()
            reach = {c.id: c.reachable() for c in cams}
            live = [c.id for c in cams if reach[c.id]]
            record["cameras"] = {c.name: {"label": self.labels.get(c.id), "reachable": reach[c.id],
                                          "scheme": c.scheme} for c in cams}
            if not live:
                # This machine's network, not a contender: a server that idles
                # for the whole run is no repetition of anything.
                raise real.RealCameraError("no camera of the scenario answers a TCP connect: nothing to measure")
            server = self.server_cls(self.dir / "server", [Camera(c.id, c.line, "") for c in cams],
                                     layout="real", h264_template=real.ffmpeg_h264_template(config))
            wants = scenarios.wants(self.data)
            server.start(wants)
            record["server"] = server.describe()
            text = server.config_text(wants)
            record["server_config"] = real.scrub(text.replace(server.password, "***") if server.password else text, cams)
            for group in self.data.get("preloads") or []:
                for cam in scenarios.camera_list(group, self.count, self.ids):
                    if cam in live:
                        stream = server.stream(cam, sub=bool(group.get("sub"))) + group.get("suffix", "")
                        preloads.append(Preload(f"cam{cam}", stream, self.dir / "clients", keep=True,
                                                queries=list(group.get("queries", [""]))).start(server))
            sampler = TreeSampler(server.pid).start()
            t0 = time.time()
            for group in self.data.get("rtsp") or []:
                for cam in scenarios.camera_list(group, self.count, self.ids):
                    if cam not in live:
                        continue
                    for k in range(int(group.get("per_camera", 1))):
                        stream = server.stream(cam, sub=bool(group.get("sub"))) + group.get("suffix", "")
                        label = reader_label(cam, bool(group.get("sub")), group.get("suffix", ""), k)
                        readers.append(RtspReader(label, server.rtsp_url(stream), self.duration,
                                                  self.dir / "clients", frames=bool(group.get("frames")),
                                                  retry=True).start())
            groups = self._browsers(server, live)
            viewers = [v for group, _ in groups for v in group]
            raw: dict = {}
            if viewers:
                origin = "/".join(server.whep_url("x").split("/", 3)[:3])
                channel = os.environ.get("OXRTC_BROWSER_CHANNEL", "msedge")
                for k, (group, tile) in enumerate(groups):
                    where = self.dir / "clients" / (f"browser-{k + 1}" if len(groups) > 1 else "")
                    probes.append(BrowserProbe(where, origin, group, self.duration, channel=channel,
                                               tile_width=tile).start())
                raw = merge_probes([p.wait() for p in probes])
            else:
                time.sleep(max(0.0, self.duration - (time.time() - t0)))
            record["readers"] = [r.result() for r in readers]
            record["preloads"] = [p.stop() for p in preloads]
            samples = sampler.stop()
            cam_cpu = sampler.camera_cpu_s()
            sampler = None
            record["summary"] = summarize(samples)
            record["samples_every_s"] = 5
            record["samples"] = [s for i, s in enumerate(samples) if i % 5 == 0]
            record["transcode_cpu_s_by_camera"] = cam_cpu
            record["server_alive_at_end"] = server.child.alive() if server.child else False
            record["browser"] = self._browser(raw, {r.get("label"): r.get("per_s") for r in record["readers"]})
            record["per_camera"] = self._per_camera(record, cams, live)
            record["status"] = "ok"
            problems = []
            if viewers and (raw.get("error") or len(raw.get("viewers") or []) != len(viewers)):
                problems.append(f"browser probe: {raw.get('error') or 'viewers missing'}")
            if not record["server_alive_at_end"]:
                problems.append("server under test exited during the run")
            if problems:
                record["status"] = "error"
                record["error"] = "; ".join(problems)[:2000]
        except Exception as err:  # noqa: BLE001 - a failed run is recorded, then the next one runs
            record["status"] = "error"
            record["error"] = f"{type(err).__name__}: {err}"[:2000]
        finally:
            if sampler is not None:
                record.setdefault("samples", sampler.stop())
            for probe in probes:
                probe.stop()
            for r in readers:
                if r.child:
                    r.child.stop()
            for p in preloads:
                p.stop()
            if server is not None:
                server.stop()
            record["ended_at"] = datetime.now(UTC).isoformat()
        self._write(record, cams)
        time.sleep(SETTLE_S)  # the cameras ration connections: let this run's go
        return record

    def _write(self, record: dict, cams: list[real.RealCamera]) -> None:
        """The run's JSON, scrubbed; a leak is checked on what is about to be written."""
        clean = real.scrub_obj(record, cams)
        text = json.dumps(clean)
        text = real.scrub(text, cams)
        leaked = real.leaks(text, cams)
        if leaked:  # cannot happen after scrub(); written down loudly if it ever does
            raise RuntimeError(f"a camera's address survived the scrub: {leaked}")
        self.json_path.parent.mkdir(parents=True, exist_ok=True)
        self.json_path.write_text(text, encoding="utf-8")

    def _browsers(self, server, live: list[int]) -> list[tuple[list[Viewer], int | None]]:
        """The viewers of each browser process, with its tiles' width: one browser
        holding every viewer, and for a group that says `browsers: N` (RC5, the
        real case) N more, each holding a copy of that group's viewers - N people
        on N devices, each with openIVR's live grid open."""
        shared: list[Viewer] = []
        own: list[tuple[list[Viewer], int | None]] = []
        for group in self.data.get("webrtc") or []:
            browsers = int(group.get("browsers", 0))
            if not browsers:
                shared += self._viewers(server, live, group)
                continue
            for b in range(browsers):
                own.append((self._viewers(server, live, group, f"b{b + 1}-"), group.get("tile_px")))
        return ([(shared, None)] if shared else []) + own

    def _viewers(self, server, live: list[int], group: dict, tag: str = "") -> list[Viewer]:
        viewers = []
        want = group.get("want", "copy")
        for cam in scenarios.camera_list(group, self.count, self.ids):
            if cam not in live:
                continue
            for k in range(int(group.get("per_camera", 1))):
                stream = server.stream(cam, want, sub=bool(group.get("sub")))
                viewers.append(Viewer(
                    label=f"cam{cam}-{want}-{tag}{k + 1}", url=server.whep_url(stream),
                    headers=server.auth_header(), reference_width=1, decode=False,
                    cycles=int(group.get("cycles", 0)),
                    hold_after_first_frame_ms=int(group.get("hold_ms", 1000)),
                    gap_ms=int(group.get("gap_ms", 0)), spread_ms=int(group.get("spread_ms", 0)),
                    reconnect=bool(group.get("reconnect", False)),
                ))
        return viewers

    def _browser(self, raw: dict, recorder_fps: dict | None = None) -> dict:
        viewers = []
        for v in raw.get("viewers") or []:
            figures = analyze_viewer(v, None, 40.0)
            if len(v.get("attempts") or []) <= 1:  # one session: its freezes mean something
                cam = (v.get("label") or "").split("-")[0][3:]
                source = (recorder_fps or {}).get(f"cam{cam}-rec-1") or stats_fps(v)
                threshold = freeze_threshold_ms(source)
                figures["freeze_threshold_ms"] = threshold
                figures.update(freeze_figures(v.get("frames") or [], threshold))
                figures["decoded_fps"] = stats_fps(v)
            figures["frames_decoded"] = (figures.get("video_stats_last") or {}).get("framesDecoded")
            viewers.append(figures)
        return {
            "error": raw.get("error"), "channel": raw.get("channel"), "ua": raw.get("ua"),
            "clock_skew_ms": [raw.get("clockSkewStartMs"), raw.get("clockSkewEndMs")],
            "console_errors": (raw.get("consoleErrors") or [])[:10],
            "viewers": viewers,
        }

    def _per_camera(self, record: dict, cams: list[real.RealCamera], live: list[int]) -> dict:
        """One entry per camera: what its viewers saw, what the recorder read, what its transcoder cost."""
        out: dict = {}
        viewers = (record.get("browser") or {}).get("viewers") or []
        readers = {r.get("label"): r for r in record.get("readers") or []}
        cpu = record.get("transcode_cpu_s_by_camera") or {}
        for cam in cams:
            mine = [v for v in viewers if (v.get("label") or "").startswith(f"cam{cam.id}-")]

            def avg(key: str, rows=mine):
                values = [r[key] for r in rows if r.get(key) is not None]
                return sum(values) / len(values) if values else None

            first = [f for v in mine for f in v.get("first_frame_ms") or []]
            rec = readers.get(reader_label(cam.id, False, "_rec", 0)) or {}
            entry = {
                "label": self.labels.get(cam.id),
                "reachable": cam.id in live,
                "viewers": len(mine),
                "viewers_without_a_frame": sum(1 for v in mine if not v.get("frames_presented")),
                "attempts": sum(v.get("attempts") or 0 for v in mine),
                "failed_attempts": sum(v.get("failed_attempts") or 0 for v in mine),
                "first_frame_ms": first,
                "fps": avg("fps_steady"),
                "decoded_fps": avg("decoded_fps"),
                "freezes_1s": avg("freezes_1s"),
                "frozen_s": avg("frozen_s"),
                "longest_gap_ms": max([v["longest_gap_ms"] for v in mine if v.get("longest_gap_ms") is not None],
                                      default=None),
                "recorder_fps": rec.get("per_s"),
                "recorder_error": rec.get("error"),
                # The work done (W6): what the transcode sent, what each consumer was given.
                "video_kbps": avg("video_kbps"),
                "frames_decoded": avg("frames_decoded"),
                "viewer_tracks": sorted({t for v in mine for t in v.get("tracks") or []}),
                "recorder_tracks": rec.get("tracks"),
                "transcode_cpu_pct_core": (100.0 * cpu[cam.name] / self.duration) if cam.name in cpu else None,
                "error": next((v["error"] for v in mine if v.get("error")), None),
            }
            out[cam.name] = entry
        return out
