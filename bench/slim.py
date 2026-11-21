"""A run's JSON without its bulk: what a published session's raw/ folder holds.

Usage:
    python bench/slim.py <session-dir> [<session-dir> ...] --out <dir>

Each run file of each session (every `*.json` but `session.json`) is written
to `<dir>/<its name>` without:

- `samples`: the sampler's series (one entry a second or every five);
- `server_config`: the configuration the server under test was given, which
  holds its ports and, for oxrtc, the run's API user;
- `browser.raw`: the probe's dump (every presented frame of every viewer,
  10 MB for a run of B05).

Everything the tables read stays: `summary`, `per_camera`, `readers`,
`browser.viewers` (the figures computed from the dump), `scenario`, `server`,
`machine`, `background`. Two keys are added: `scenario_id`, and `oxrtc_mode`
(bench/work.py), the mode read from the configuration that is left out, so that
`bench/tables.py` run on a slim session names the same mode. A slim session
gives the same tables as the full one.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import work  # noqa: E402

#: Top-level keys left out.
DROPPED = ("samples", "server_config")
#: Keys of `browser` left out.
DROPPED_BROWSER = ("raw",)


def slim(record: dict) -> dict:
    """A copy of one run's record without its bulk (see the module docstring)."""
    out = {k: v for k, v in record.items() if k not in DROPPED}
    if isinstance(record.get("browser"), dict):
        out["browser"] = {k: v for k, v in record["browser"].items() if k not in DROPPED_BROWSER}
    out["scenario_id"] = (record.get("scenario") or {}).get("id")
    name = (record.get("server") or {}).get("name") or ""
    if name.startswith("oxrtc"):
        out["oxrtc_mode"] = work.oxrtc_mode(record)
    return out


def run_files(session: Path) -> list[Path]:
    return sorted(p for p in session.glob("*.json") if p.name != "session.json")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("sessions", type=Path, nargs="+")
    parser.add_argument("--out", type=Path, required=True)
    ns = parser.parse_args(argv)
    ns.out.mkdir(parents=True, exist_ok=True)
    before = after = count = 0
    for session in ns.sessions:
        for path in run_files(session):
            text = path.read_text(encoding="utf-8")
            small = json.dumps(slim(json.loads(text)), indent=1)
            (ns.out / path.name).write_text(small + "\n", encoding="utf-8")
            before, after, count = before + len(text), after + len(small) + 1, count + 1
    print(f"{count} runs: {before / 1e6:.1f} MB -> {after / 1e6:.2f} MB in {ns.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
