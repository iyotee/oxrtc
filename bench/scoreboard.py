"""The scoreboard of a session: oxrtc's rows won, tied, tied behind, lost (D29).

Usage:
    python bench/scoreboard.py <results.csv>

Reads the CSV that `bench/tables.py` writes (one row per scenario x metric,
the verdict in its last columns) and counts, for oxrtc against go2rtc and
MediaMTX, by the bench's own verdict (a 95 % interval clear of every other's
wins, overlapping intervals are no difference):

- **won**: "oxrtc better";
- **tied**: no difference, oxrtc among the best (it is in the verdict's list);
- **tied behind**: no difference between others that are better than oxrtc;
- **lost**: another contender better;
- **without verdict**: an information row (no better side), fewer than two
  contenders with a value, a single run, or oxrtc with no value.

The stress rows (B05, B06: 100 and 50 viewers in one browser) are counted
apart and never in the D29 count (the owner, 2026-11-19: two to five viewers
at most, each in a browser of its own; D29 as amended, D35). The lines of the
board say the D29 count, the stress rows, and all rows together. Then every
lost and tied-behind row with the three values and the oxrtc mode its runs
used (the owner, 2026-11-20: each mode against its peer, D30).

`bench/tables.py` prints this board at the top of the tables it writes.
"""

from __future__ import annotations

import csv
import sys
from collections import Counter
from pathlib import Path

#: The contenders' column titles in the CSV, oxrtc's among them.
TITLES = ("go2rtc", "oxrtc", "MediaMTX")
OXRTC = "oxrtc"
#: Scenarios whose rows are stress rows: run, reported, never in the count (D35).
STRESS = frozenset({"B05", "B06"})
#: The classes, in the order the board says them.
CLASSES = ("won", "tied", "tied behind", "lost", "without verdict")
#: The CSV column naming the oxrtc mode of a row's runs (absent from older files).
MODE = "oxrtc mode"
NOT_RECORDED = "not recorded"


def tied_names(verdict: str) -> list[str] | None:
    """The contenders of a "no difference (a, b)" verdict, None for any other."""
    head, sep, rest = verdict.partition("(")
    if head.strip() != "no difference" or not sep or not rest.endswith(")"):
        return None
    return [name.strip() for name in rest[:-1].split(",")]


def has_value(row: dict, title: str) -> bool:
    return bool((row.get(f"{title} mean") or "").strip())


def classify(row: dict) -> str:
    """One CSV row's class (one of CLASSES)."""
    verdict = (row.get("verdict") or "").strip()
    if not has_value(row, OXRTC):
        return "without verdict"
    if verdict == f"{OXRTC} better":
        return "won"
    names = tied_names(verdict)
    if names is not None:
        return "tied" if OXRTC in names else "tied behind"
    if verdict.endswith(" better"):
        return "lost"
    return "without verdict"


def is_stress(row: dict) -> bool:
    return (row.get("scenario") or "") in STRESS


def counts(rows: list[dict]) -> Counter:
    tally = Counter({name: 0 for name in CLASSES})
    tally.update(classify(row) for row in rows)
    return tally


def count_line(tally: Counter) -> str:
    return ", ".join(f"{tally[name]} {name}" for name in CLASSES)


def value(row: dict, title: str) -> str:
    mean = (row.get(f"{title} mean") or "").strip()
    if not mean:
        note = (row.get(f"{title} note") or "").strip()
        return note[:60] or "-"
    ci = (row.get(f"{title} ci95") or "").strip()
    digits = 1 if abs(float(mean)) >= 10 else 2
    text = f"{float(mean):.{digits}f}"
    return f"{text} ± {float(ci):.{digits}f}" if ci else f"{text} (n=1)"


def detail_lines(rows: list[dict], which: str) -> list[str]:
    chosen = [r for r in rows if classify(r) == which]
    if not chosen:
        return [f"No {which} row."]
    heads = " | ".join(TITLES)
    lines = [f"| Scenario | Row | Unit | {heads} | Verdict | oxrtc mode |",
             "|---|---|---|" + "---|" * len(TITLES) + "---|---|"]
    for r in chosen:
        cells = " | ".join(value(r, t) for t in TITLES)
        stress = " (stress)" if is_stress(r) else ""
        mode = (r.get(MODE) or "").strip() or NOT_RECORDED
        lines.append(f"| {r.get('scenario')}{stress} | {r.get('metric')} | {r.get('unit')} | {cells} | "
                     f"{r.get('verdict')} | {mode} |")
    return lines


def board(rows: list[dict]) -> list[str]:
    """The scoreboard as Markdown lines."""
    counted = [r for r in rows if not is_stress(r)]
    stress = [r for r in rows if is_stress(r)]
    lines = [
        "### Scoreboard (D29: oxrtc against go2rtc and MediaMTX)",
        "",
        f"- **Counted rows (D29), {len(counted)}: {count_line(counts(counted))}**",
        f"- Stress rows ({', '.join(sorted(STRESS))}; D35, outside the count), {len(stress)}: "
        f"{count_line(counts(stress))}",
        f"- All rows, {len(rows)}: {count_line(counts(rows))}",
        "",
        "Lost rows:",
        "",
        *detail_lines(rows, "lost"),
        "",
        "Tied-behind rows:",
        "",
        *detail_lines(rows, "tied behind"),
    ]
    return lines


def read(path: Path) -> list[dict]:
    with path.open(newline="", encoding="utf-8") as fh:
        return list(csv.DictReader(fh))


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    print("\n".join(board(read(Path(args[0])))))
    return 0


if __name__ == "__main__":
    sys.exit(main())
