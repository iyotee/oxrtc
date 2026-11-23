"""Each mode against its peer (the owner, 2026-11-20; D30): one table from two sessions.

Usage:
    python bench/peers.py <default-real> <default-synthetic> <live-real> <live-synthetic> --out-md <file>

go2rtc does not hold its picture for the sound in openIVR's layout, MediaMTX
does. So a row of a scenario the live sessions ran (oxrtc with
`webrtc.lip_sync: false`, `OXRTC_BENCH_LIP_SYNC=0`) is judged twice, each pair
inside one session:

- **against go2rtc**: oxrtc in live mode, against the go2rtc runs of the same
  live session (interleaved with them);
- **against MediaMTX**: oxrtc's default (lip sync on), against the MediaMTX runs
  of the default session.

Each pair's verdict is `bench/tables.py`'s own (95 % Student intervals, a side
better only when its interval is clear of the other's). The default sessions'
three-way tables and their scoreboard (D29) are not changed by this table: it is
printed beside them, so that a row lost to go2rtc in the default mode can be read
against the mode the owner compares with go2rtc.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import tables  # noqa: E402
from tables import Row  # noqa: E402


def pair(row: Row, other: str, oxrtc_runs) -> Row:
    """A copy of `row` with two contenders only: `other`'s runs and the given oxrtc runs."""
    out = Row(row.table, row.scenario, row.camera, row.label, row.unit, row.lower, row.digits)
    out.per_run = {other: row.per_run.get(other, []), "oxrtc": oxrtc_runs}
    out.mode = row.mode
    return out


def key(row: Row) -> tuple:
    return (row.table, row.scenario, row.camera, row.label, row.unit)


def peer_rows(default_rows: list[Row], live_rows: list[Row]) -> list[tuple[Row, Row, Row]]:
    """(default row, live row, ...) -> for each row of a scenario both sessions have:
    (the live row's go2rtc vs live oxrtc pair, the default row's MediaMTX vs default oxrtc pair,
    the default row itself)."""
    live = {key(r): r for r in live_rows if any(r.per_run.get("oxrtc") or [])}
    out = []
    for row in default_rows:
        twin = live.get(key(row))
        if twin is None or row.lower is None:
            continue
        out.append((pair(twin, "go2rtc", twin.per_run.get("oxrtc", [])),
                    pair(row, "mediamtx", row.per_run.get("oxrtc", [])), row))
    return out


def cell(row: Row, name: str) -> str:
    iv = tables.interval(row, name)
    if iv is None:
        return tables.cell_text(row, name, {})
    if iv.half_width is None:
        return f"{iv.mean:.{row.digits}f} (n=1)"
    return f"{iv.mean:.{row.digits}f} ± {iv.half_width:.{row.digits}f}"


def render(triples: list[tuple[Row, Row, Row]]) -> list[str]:
    lines = ["| Scenario | Row | Unit | go2rtc | oxrtc live | verdict | oxrtc default | MediaMTX | verdict |",
             "|---|---|---|---|---|---|---|---|---|"]
    for vs_go, vs_mtx, row in triples:
        lines.append(f"| {row.scenario} | {row.label} | {row.unit} | {cell(vs_go, 'go2rtc')} | {cell(vs_go, 'oxrtc')} "
                     f"| {tables.verdict(vs_go)[0]} | {cell(vs_mtx, 'oxrtc')} | {cell(vs_mtx, 'mediamtx')} "
                     f"| {tables.verdict(vs_mtx)[0]} |")
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("default_real", "default_synthetic", "live_real", "live_synthetic"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--out-md", type=Path, required=True)
    ns = parser.parse_args(argv)

    def rows(real: Path, synthetic: Path) -> list[Row]:
        r, s = tables.by_scenario(tables.load(real)), tables.by_scenario(tables.load(synthetic))
        out_real, out_syn = tables.rows_real(r), tables.rows_synthetic(s)
        tables.set_modes(out_real, r)
        tables.set_modes(out_syn, s)
        return out_real + out_syn

    triples = peer_rows(rows(ns.default_real, ns.default_synthetic), rows(ns.live_real, ns.live_synthetic))
    ns.out_md.write_text("\n".join(render(triples)) + "\n", encoding="utf-8")
    print(f"{len(triples)} rows judged against each peer")
    return 0


if __name__ == "__main__":
    sys.exit(main())
