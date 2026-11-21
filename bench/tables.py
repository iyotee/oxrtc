"""The final two-way tables: one row per scenario x metric, one column per contender.

Usage:
    python bench/tables.py <real-session> <synthetic-session> --out-md tables.md --out-csv results.csv

Each run contributes ONE value per row (its own mean over the run); a cell is
the mean of those values across repetitions, `± the half-width of the 95 %
Student interval (n runs)`. The winner of a row is in bold; when the best two
intervals overlap the verdict says "no difference". A cell with no value says
why: `failed: <reason>`, `unreachable`, `unsupported: <reason>`, `not run`.
Nothing here reads a median. The markdown and the CSV are generated from the
raw JSON, never edited by hand.
"""

from __future__ import annotations

import argparse
import csv
import json
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import report  # noqa: E402
import scoreboard  # noqa: E402
import work  # noqa: E402
from stats import Interval, mean_ci  # noqa: E402

#: (adapter name, column title)
CONTENDERS = [("go2rtc", "go2rtc"), ("oxrtc", "oxrtc"), ("mediamtx", "MediaMTX")]

LOW, HIGH, INFO = True, False, None
UNREACHABLE = "camera did not answer the preflight"


@dataclass
class Row:
    table: str
    scenario: str
    camera: str
    label: str
    unit: str
    lower: bool | None
    digits: int
    #: contender -> one (value, reason) per run
    per_run: dict[str, list[tuple[float | None, str | None]]] = field(default_factory=dict)
    #: the oxrtc mode its runs used (work.oxrtc_mode), "" when oxrtc did not run
    mode: str = ""


def load(session: Path | None) -> list[dict]:
    return report.load(session) if session and session.is_dir() else []


def by_scenario(runs: list[dict]) -> dict[str, dict[str, list[dict]]]:
    out: dict[str, dict[str, list[dict]]] = defaultdict(lambda: defaultdict(list))
    for run in runs:
        name = (run.get("server") or {}).get("name") or run["run_id"].split("_")[1]
        out[run["scenario"]["id"]][name].append(run)
    return out


def _mean(values):
    values = [v for v in values if v is not None]
    return sum(values) / len(values) if values else None


def run_error(run: dict) -> str | None:
    if run.get("status") == "ok":
        return None
    return (run.get("error") or "the run failed").replace("\n", " ")[:160]


# -- extractors: run -> (value, reason) -------------------------------------


def summary_value(key: str):
    def get(run: dict):
        err = run_error(run)
        value = (run.get("summary") or {}).get(key)
        return (None, err) if err else (value, None if value is not None else "no sample")

    return get


def camera_value(camera: str, key: str, scale: float = 1.0):
    """A per-camera figure of a real run; None with the reason when the camera gave none."""

    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        entry = (run.get("per_camera") or {}).get(camera) or {}
        if not entry.get("reachable"):
            return None, UNREACHABLE
        if entry.get("viewers") and entry.get("viewers_without_a_frame") == entry.get("viewers"):
            return None, entry.get("error") or "no picture"
        value = entry.get(key)
        return (None if value is None else value * scale), (None if value is not None else "no figure")

    return get


def camera_first_frame(camera: str):
    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        entry = (run.get("per_camera") or {}).get(camera) or {}
        if not entry.get("reachable"):
            return None, UNREACHABLE
        first = entry.get("first_frame_ms") or []
        if not first:
            return None, entry.get("error") or "no first frame"
        return sum(first) / len(first), None

    return get


def camera_failed_attempts(camera: str):
    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        entry = (run.get("per_camera") or {}).get(camera) or {}
        if not entry.get("reachable"):
            return None, UNREACHABLE
        return float(entry.get("failed_attempts") or 0), None

    return get


def camera_of_source(camera: str, source: dict[str, float | None]):
    """Frames decoded as a share of the camera's source rate (the recorder's rate, pooled over contenders)."""

    def get(run: dict):
        value, reason = camera_value(camera, "decoded_fps")(run)
        rate = source.get(camera)
        if value is None or not rate:
            return None, reason or "no source rate"
        return 100.0 * value / rate, None

    return get


def all_first_frames(run: dict):
    err = run_error(run)
    if err:
        return None, err
    firsts = [f for e in (run.get("per_camera") or {}).values() for f in e.get("first_frame_ms") or []]
    return (sum(firsts) / len(firsts), None) if firsts else (None, "no first frame")


def all_failed_attempts(run: dict):
    err = run_error(run)
    if err:
        return None, err
    return float(sum(e.get("failed_attempts") or 0 for e in (run.get("per_camera") or {}).values())), None


def viewers_with_a_picture(run: dict):
    err = run_error(run)
    if err:
        return None, err
    cams = [e for e in (run.get("per_camera") or {}).values() if e.get("reachable")]
    total = sum(e.get("viewers") or 0 for e in cams)
    blind = sum(e.get("viewers_without_a_frame") or 0 for e in cams)
    return (float(total - blind), None) if total else (None, "no viewer")


def synthetic_metric(key: str):
    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        value = report.run_metrics(run).get(key)
        return value, None if value is not None else "no figure"

    return get


def synthetic_recovery(index: int):
    def get(run: dict):
        err = run_error(run)
        if err:
            return None, err
        values = [v["recovery_ms"][index] for v in (run.get("browser") or {}).get("viewers") or []
                  if len(v.get("recovery_ms") or []) > index and v["recovery_ms"][index] is not None]
        return (sum(values) / len(values), None) if values else (None, "no frame after the camera returned")

    return get


# -- rows ---------------------------------------------------------------------

TOTAL = "all cameras"


def source_rates(rc1: dict[str, list[dict]]) -> dict[str, float | None]:
    """Each camera's source rate: the highest mean rate any contender's recorder read on camera_N_rec in RC1.

    A server cannot deliver more pictures than its camera sends, so the best
    recorder reading is the least wrong estimate of the source; a contender
    whose recorder reads less (MediaMTX on the MJPEG webcams) is shown below it.
    """
    best: dict[str, float] = {}
    for runs in rc1.values():
        per_camera: dict[str, list[float]] = defaultdict(list)
        for run in runs:
            if run.get("status") != "ok":
                continue
            for name, entry in (run.get("per_camera") or {}).items():
                if entry.get("recorder_fps"):
                    per_camera[name].append(entry["recorder_fps"])
        for name, values in per_camera.items():
            best[name] = max(best.get(name, 0.0), sum(values) / len(values))
    return best


def unreachable_everywhere(runs_by_c: dict[str, list[dict]], camera: str) -> bool:
    """True when no run of any contender could connect to the camera."""
    seen = [bool((r.get("cameras") or {}).get(camera, {}).get("reachable"))
            for runs in runs_by_c.values() for r in runs]
    return bool(seen) and not any(seen)


def camera_names(runs_by_c: dict[str, list[dict]]) -> list[tuple[str, str]]:
    """[(camera name, label)] in scenario order, from any run that has them."""
    for runs in runs_by_c.values():
        for run in runs:
            ids = (run.get("scenario") or {}).get("cameras", {}).get("ids") or []
            labels = (run.get("scenario") or {}).get("cameras", {}).get("labels") or {}
            if ids:
                return [(f"camera_{i}", str(labels.get(i) or labels.get(str(i)) or "")) for i in ids]
    return []


def work_rows(add, scenario: str, camera: str | None = TOTAL) -> None:
    """The rows beside the raw CPU rows (W6): the cost per delivered frame, the
    server split from its FFmpeg children, and the tree while every transcode runs."""
    where = (scenario, camera) if camera is not None else (scenario,)
    add(*where, "frames delivered (pictures decoded by the viewers + read by the recorders)", "frames", INFO, 0,
        lambda run: (work.frames_delivered(run), work.run_error(run) or (None if work.frames_delivered(run)
                                                                          else "no figure")))
    for key, label in (("cpu_server_pct_core_mean", "server process CPU"),
                       ("cpu_children_pct_core_mean", "its FFmpeg children's CPU"),
                       ("cpu_pct_core_mean", "server tree CPU")):
        add(*where, f"{label} per delivered frame", "ms of one core", LOW, 3, work.cpu_per_frame(key))
    for key, label, unit in (("cpu_children_pct_core", "its FFmpeg children's CPU", "% of one core"),
                             ("cpu_pct_core", "server tree CPU", "% of one core"),
                             ("rss_children_mb", "its FFmpeg children's RSS", "MiB")):
        add(*where, f"{label} while every transcode runs", unit, LOW, 1, work.all_running_value(key))


def rows_real(real: dict[str, dict[str, list[dict]]]) -> list[Row]:
    rows: list[Row] = []

    def add(scenario, camera, label, unit, lower, digits, getter):
        row = Row("real cameras", scenario, camera, label, unit, lower, digits)
        for name, _ in CONTENDERS:
            row.per_run[name] = [getter(run) for run in real.get(scenario, {}).get(name, [])]
        rows.append(row)

    rate = source_rates(real.get("RC1", {}))
    for scenario in ("RC1", "RC4"):
        if scenario not in real:
            continue
        for cam, kind in camera_names(real[scenario]):
            what = cam
            if unreachable_everywhere(real[scenario], cam):
                add(scenario, cam, f"{cam}: every figure", "-", INFO, 0, camera_value(cam, "decoded_fps"))
                continue
            add(scenario, cam, f"{what}: frames decoded per s", "fps", HIGH, 1, camera_value(cam, "decoded_fps"))
            add(scenario, cam, f"{what}: share of the source rate", "%", HIGH, 1, camera_of_source(cam, rate))
            if scenario == "RC1":
                add(scenario, cam, f"{what}: recorder frames read per s", "fps", HIGH, 1,
                    camera_value(cam, "recorder_fps"))
            add(scenario, cam, f"{what}: freezes > 1 s per viewer", "count", LOW, 1, camera_value(cam, "freezes_1s"))
            add(scenario, cam, f"{what}: longest gap between frames", "ms", LOW, 0, camera_value(cam, "longest_gap_ms"))
            add(scenario, cam, f"{what}: first frame", "ms", LOW, 0, camera_first_frame(cam))
            add(scenario, cam, f"{what}: its FFmpeg children's CPU", "% of one core", LOW, 1,
                camera_value(cam, "transcode_cpu_pct_core"))
            add(scenario, cam, f"{what}: its FFmpeg children's CPU per picture its viewer decoded",
                "ms of one core", LOW, 2, work.camera_cpu_per_picture(cam))
            add(scenario, cam, f"{what}: picture bitrate its viewer received", "kbit/s", INFO, 0,
                camera_value(cam, "video_kbps"))
        add(scenario, TOTAL, "viewers that showed a picture", "viewers", HIGH, 1, viewers_with_a_picture)
        add(scenario, TOTAL, "server process CPU", "% of one core", LOW, 1, summary_value("cpu_server_pct_core_mean"))
        add(scenario, TOTAL, "its FFmpeg children's CPU", "% of one core", LOW, 1,
            summary_value("cpu_children_pct_core_mean"))
        add(scenario, TOTAL, "server tree CPU (process + children)", "% of one core", LOW, 1,
            summary_value("cpu_pct_core_mean"))
        add(scenario, TOTAL, "server tree CPU", "% of the machine", LOW, 2, summary_value("cpu_pct_machine_mean"))
        work_rows(add, scenario)
        add(scenario, TOTAL, "server process RSS", "MiB", LOW, 1, summary_value("rss_server_mb_mean"))
        add(scenario, TOTAL, "its FFmpeg children's RSS", "MiB", LOW, 1, summary_value("rss_children_mb_mean"))
        add(scenario, TOTAL, "server tree RSS (mean)", "MiB", LOW, 1, summary_value("rss_mb_mean"))
        add(scenario, TOTAL, "transcoders (FFmpeg processes), mean", "processes", INFO, 1,
            summary_value("ffmpeg_procs_mean"))
        add(scenario, TOTAL, "transcoders (FFmpeg processes), max", "processes", INFO, 0,
            summary_value("ffmpeg_procs_max"))
        add(scenario, TOTAL, "machine CPU during the run", "%", INFO, 1, summary_value("machine_cpu_pct_mean"))
    for scenario in ("RC2", "RC3"):
        if scenario not in real:
            continue
        kind = "warm" if scenario == "RC2" else "cold"
        for cam, label in camera_names(real[scenario]):
            what = cam
            if unreachable_everywhere(real[scenario], cam):
                add(scenario, cam, f"{cam}: every figure", "-", INFO, 0, camera_first_frame(cam))
                continue
            add(scenario, cam, f"{what}: {kind} first frame", "ms", LOW, 0, camera_first_frame(cam))
            add(scenario, cam, f"{what}: viewers without a picture", "count", LOW, 1, camera_failed_attempts(cam))
        add(scenario, TOTAL, f"{kind} first frame, every attempt", "ms", LOW, 0, all_first_frames)
        add(scenario, TOTAL, "viewers without a picture", "count", LOW, 1, all_failed_attempts)
        add(scenario, TOTAL, "server tree CPU", "% of one core", LOW, 1, summary_value("cpu_pct_core_mean"))
        add(scenario, TOTAL, "server tree RSS (mean)", "MiB", LOW, 1, summary_value("rss_mb_mean"))
    return rows


def rows_synthetic(syn: dict[str, dict[str, list[dict]]]) -> list[Row]:
    rows: list[Row] = []

    def add(scenario, label, unit, lower, digits, getter):
        row = Row("synthetic", scenario, "", label, unit, lower, digits)
        for name, _ in CONTENDERS:
            row.per_run[name] = [getter(run) for run in syn.get(scenario, {}).get(name, [])]
        rows.append(row)

    def metric(sid, key, label, unit, lower, digits):
        add(sid, label, unit, lower, digits, synthetic_metric(key))

    if "L01" in syn:
        metric("L01", "latency_ms_mean", "glass-to-glass latency (mean)", "ms", LOW, 1)
        metric("L01", "latency_ms_p95", "glass-to-glass latency (P95)", "ms", LOW, 1)
        metric("L01", "sound_held_ms", "sound held by the browser", "ms", LOW, 1)
        metric("L01", "picture_asked_ms", "picture held for the sound (browser's target beyond the network's)", "ms", INFO, 1)
        metric("L01", "sound_behind_ms", "sound behind the picture (by the server's dates)", "ms", INFO, 1)
        metric("L01", "av_offset_ms", "sound behind the picture (beep heard minus flash seen)", "ms", INFO, 1)
        metric("L01", "sound_concealed_pct", "sound concealed (browser)", "% of the samples", LOW, 2)
        metric("L01", "first_frame_ms", "first frame", "ms", LOW, 0)
        metric("L01", "presented_fps", "frames presented", "fps", HIGH, 2)
        metric("L01", "frames_dropped", "frames dropped (browser)", "frames", LOW, 1)
        metric("L01", "cpu_pct_core", "server tree CPU", "% of one core", LOW, 2)
        add("L01", "server tree CPU per delivered frame", "ms of one core", LOW, 3,
            work.cpu_per_frame("cpu_pct_core_mean"))
        metric("L01", "rss_mb", "server tree RSS (mean)", "MiB", LOW, 1)
    for sid in ("R01", "R02"):
        if sid not in syn:
            continue
        sample = next((r for runs in syn[sid].values() for r in runs if r.get("scenario")), None)
        faults = sorted((sample or {}).get("scenario", {}).get("faults") or [], key=lambda f: f["at_s"])
        for i, fault in enumerate(faults):
            add(sid, f"recovery after a {fault['down_s']} s outage (camera back -> frame)", "ms", LOW, 0,
                synthetic_recovery(i))
        metric(sid, "cpu_pct_core", "server tree CPU", "% of one core", LOW, 2)
        metric(sid, "rss_mb", "server tree RSS (mean)", "MiB", LOW, 1)
    for sid in ("B04", "B05", "B06"):
        if sid not in syn:
            continue
        metric(sid, "cpu_pct_core", "server tree CPU", "% of one core", LOW, 1)
        metric(sid, "cpu_pct_machine", "server tree CPU", "% of the machine", LOW, 2)
        work_rows(add, sid, None)
        metric(sid, "rss_mb", "server tree RSS (mean)", "MiB", LOW, 1)
        metric(sid, "rss_mb_max", "server tree RSS (max)", "MiB", LOW, 1)
        metric(sid, "first_frame_ms", "first frame", "ms", LOW, 0)
        metric(sid, "presented_fps", "frames presented", "fps", HIGH, 2)
        metric(sid, "frames_dropped", "frames dropped (browser)", "frames", LOW, 1)
        metric(sid, "freezes", "freezes (browser)", "count", LOW, 1)
        metric(sid, "machine_cpu", "machine CPU during the run", "%", INFO, 1)
    return rows


# -- cells ------------------------------------------------------------------------


def interval(row: Row, name: str) -> Interval | None:
    return mean_ci([v for v, _ in row.per_run.get(name, []) if v is not None])


def runs_of(row: Row, name: str) -> int:
    return len(row.per_run.get(name, []))


def one_line(reason: str) -> str:
    """A reason as one table cell: a client's error can carry FFmpeg's stderr
    (newlines, CRs), which split a Markdown row in two (RC1, go2rtc's camera_4
    recorder, 2026-11-19); a pipe would end the cell."""
    return " ".join(reason.split()).replace("|", "/")


def cell_text(row: Row, name: str, unsupported: dict[str, str]) -> str:
    runs = row.per_run.get(name, [])
    if not runs:
        return f"unsupported: {unsupported[name]}" if name in unsupported else "not run"
    values = [v for v, _ in runs if v is not None]
    reasons = Counter(one_line(r) for v, r in runs if v is None and r)
    if not values:
        why = "; ".join(f"{r} ({n} of {len(runs)} runs)" for r, n in reasons.items())
        if set(reasons) == {UNREACHABLE}:
            return "unreachable"
        return f"failed: {why}"
    iv = mean_ci(values)
    assert iv is not None
    if iv.half_width is None:
        text = f"{iv.mean:.{row.digits}f} (n=1, no CI)"
    else:
        text = f"{iv.mean:.{row.digits}f} ± {iv.half_width:.{row.digits}f} (n={iv.n})"
    if reasons:
        why = "; ".join(f"{r}: {n}" for r, n in reasons.items())
        text += f" [{len(runs) - len(values)} of {len(runs)} runs gave none: {why}]"
    return text


def verdict(row: Row) -> tuple[str, str | None]:
    """(text, winner's adapter name when it clearly wins)."""
    if row.lower is None:
        return "", None
    ivs = {n: interval(row, n) for n, _ in CONTENDERS}
    present = {n: iv for n, iv in ivs.items() if iv is not None}
    if len(present) < 2:
        return "", None
    ranked = sorted(present.items(), key=lambda kv: kv[1].mean, reverse=not row.lower)
    best_name, best = ranked[0]
    title = dict(CONTENDERS)
    if any(iv.half_width is None for iv in present.values()):
        return "n<2: no conclusion", None
    tied = [n for n, iv in ranked[1:] if best.overlaps(iv)]
    if tied:
        names = ", ".join(title[n] for n in [best_name, *tied])
        return f"no difference ({names})", None
    return f"{title[best_name]} better", best_name


def bold(text: str) -> str:
    return f"**{text}**"


# -- rendering ---------------------------------------------------------------------


def render_table(rows: list[Row], unsupported: dict[str, dict[str, str]]) -> list[str]:
    lines: list[str] = []
    current = None
    heads = " | ".join(title for _, title in CONTENDERS)
    for row in rows:
        if row.scenario != current:
            current = row.scenario
            lines += ["", f"#### {row.scenario}", "", f"| Row | Unit | {heads} | Verdict |", "|---|---|" + "---|" * (len(CONTENDERS) + 1)]
        text, winner = verdict(row)
        cells = []
        for name, _ in CONTENDERS:
            cell = cell_text(row, name, unsupported.get(row.scenario, {}))
            cells.append(bold(cell) if winner == name else cell)
        lines.append(f"| {row.label} | {row.unit} | " + " | ".join(cells) + f" | {text} |")
    return lines


def csv_records(rows: list[Row], unsupported: dict[str, dict[str, str]]) -> tuple[list[str], list[list]]:
    """The CSV's header and lines: what render_csv writes and the scoreboard reads."""
    head = ["table", "scenario", "camera", "metric", "unit", "better"]
    for _, title in CONTENDERS:
        head += [f"{title} mean", f"{title} ci95", f"{title} n", f"{title} runs", f"{title} note"]
    head += ["verdict", scoreboard.MODE]
    lines = []
    for row in rows:
        text, winner = verdict(row)
        line = [row.table, row.scenario, row.camera, row.label, row.unit,
                {True: "lower", False: "higher", None: ""}[row.lower]]
        for name, _ in CONTENDERS:
            iv = interval(row, name)
            note = cell_text(row, name, unsupported.get(row.scenario, {})) if iv is None else ""
            line += [None if iv is None else round(iv.mean, 4),
                     None if iv is None or iv.half_width is None else round(iv.half_width, 4),
                     None if iv is None else iv.n, runs_of(row, name), note]
        line += [text, row.mode]
        lines.append(line)
    return head, lines


def render_csv(rows: list[Row], path: Path, unsupported: dict[str, dict[str, str]]) -> None:
    head, lines = csv_records(rows, unsupported)
    with path.open("w", newline="", encoding="utf-8") as fh:
        writer = csv.writer(fh)
        writer.writerow(head)
        writer.writerows(lines)


def score_lines(rows: list[Row], unsupported: dict[str, dict[str, str]]) -> list[str]:
    """The scoreboard of these rows (bench/scoreboard.py), from the CSV's own records."""
    head, lines = csv_records(rows, unsupported)
    as_text = [{k: "" if v is None else str(v) for k, v in zip(head, line)} for line in lines]
    return scoreboard.board(as_text)


def set_modes(rows: list[Row], by_sid: dict[str, dict[str, list[dict]]]) -> None:
    """Each row names the oxrtc mode of its scenario's oxrtc runs (the owner, 2026-11-20, D30)."""
    for row in rows:
        runs = by_sid.get(row.scenario, {}).get("oxrtc", [])
        row.mode = " + ".join(sorted({work.oxrtc_mode(r) for r in runs}))


def machine_facts(runs: list[dict]) -> list[str]:
    first = next((r for r in runs if r.get("machine")), None)
    if not first:
        return []
    m = first["machine"]
    servers: dict[str, str] = {}
    for r in runs:
        s = r.get("server") or {}
        if s.get("name") and s.get("version"):
            servers[s["name"]] = s["version"]
    tools = first.get("tools") or {}
    lines = [
        f"- CPU: {m.get('processor')}, {m.get('cpu_physical')} cores / {m.get('cpu_logical')} threads, "
        f"max {m.get('cpu_max_mhz')} MHz; RAM {m.get('memory_total_mb')} MiB",
        f"- OS: {m.get('os')} {m.get('os_release')} ({m.get('os_version')}), Python {m.get('python')}",
    ]
    for name, title in CONTENDERS:
        if name in servers:
            lines.append(f"- {title}: {servers[name]}")
    lines += [f"- FFmpeg: {tools.get('ffmpeg')}", f"- Node: {tools.get('node')}"]
    ua = next(((r.get("browser") or {}).get("ua") for r in runs if (r.get("browser") or {}).get("ua")), None)
    if ua:
        lines.append(f"- Browser: {ua}")
    return lines


def background(runs: list[dict]) -> list[str]:
    values = [(r.get("background") or {}).get("machine_cpu_pct") for r in runs]
    values = [v for v in values if v is not None]
    if not values:
        return []
    tops: Counter = Counter()
    for r in runs:
        for item in (r.get("background") or {}).get("top") or []:
            tops[item["name"]] += item["cpu_pct_machine"]
    busiest = ", ".join(f"{n} ({v / len(values):.2f} %)" for n, v in tops.most_common(4))
    return [f"- Machine CPU in the 3 s before each run: mean {sum(values) / len(values):.1f} %, "
            f"max {max(values):.1f} % over {len(values)} runs; busiest processes (mean share of the machine): {busiest}"]


def run_lines(runs: list[dict]) -> list[str]:
    out = []
    for r in sorted(runs, key=lambda r: r["run_id"]):
        err = f" - {run_error(r)}" if run_error(r) else ""
        out.append(f"- `{r['run_id']}` {r.get('status')}, {r.get('duration_s')} s{err}")
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("real", type=Path)
    parser.add_argument("synthetic", type=Path)
    parser.add_argument("--out-md", type=Path, required=True)
    parser.add_argument("--out-csv", type=Path, required=True)
    ns = parser.parse_args(argv)
    real_runs, syn_runs = load(ns.real), load(ns.synthetic)
    real, syn = by_scenario(real_runs), by_scenario(syn_runs)
    real_rows, syn_rows = rows_real(real), rows_synthetic(syn)
    unsupported: dict[str, dict[str, str]] = {}
    set_modes(real_rows, real)
    set_modes(syn_rows, syn)
    lines = score_lines(real_rows + syn_rows, unsupported)
    lines += ["", "### Table 1 - the owner's six real cameras", ""]
    lines += [f"- `{cam}`: {label}" for cam, label in camera_names(next(iter(real.values()), {}))]
    lines += render_table(real_rows, unsupported)
    lines += ["", "### Table 2 - the synthetic rig (what real cameras cannot measure)", ""]
    lines += render_table(syn_rows, unsupported)
    lines += ["", "### Machine", ""] + machine_facts(real_runs + syn_runs) + background(real_runs + syn_runs)
    lines += ["", "### Runs", ""] + run_lines(real_runs) + run_lines(syn_runs)
    ns.out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    render_csv(real_rows + syn_rows, ns.out_csv, unsupported)
    print(f"{len(real_rows)} real rows, {len(syn_rows)} synthetic rows; {len(real_runs)} + {len(syn_runs)} runs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
