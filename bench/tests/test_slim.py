"""bench/slim.py: a run without its bulk gives the same tables."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import slim  # noqa: E402
import tables  # noqa: E402
import work  # noqa: E402


def run(name: str, rep: int, cpu: float, config: str = "webrtc:\n  lip_sync: false\n") -> dict:
    return {
        "schema": 1, "run_id": f"B04_{name}_r{rep}_x", "scenario": {"id": "B04"}, "repetition": rep,
        "server": {"name": name, "version": f"{name} 1.0"}, "status": "ok",
        "server_config": config, "samples": [{"t": i, "cpu": cpu} for i in range(100)],
        "summary": {"cpu_pct_core_mean": cpu, "rss_mb_mean": 50.0 + rep},
        "browser": {"ua": "Edge", "raw": {"viewers": [{"frames": [[0, 1, 2]] * 1000}]},
                    "viewers": [{"label": "cam1-copy-1", "presented_fps": 25.0 - rep, "first_frame_ms": [300.0]}]},
    }


class Slim(unittest.TestCase):
    def test_the_bulk_is_left_out_and_the_figures_kept(self):
        record = run("oxrtc", 1, 10.0)
        small = slim.slim(record)
        for key in ("samples", "server_config"):
            self.assertNotIn(key, small)
        self.assertNotIn("raw", small["browser"])
        self.assertEqual(small["browser"]["viewers"], record["browser"]["viewers"])
        self.assertEqual(small["summary"], record["summary"])
        self.assertEqual(small["scenario_id"], "B04")
        self.assertIn("samples", record, "the record itself is not changed")

    def test_the_oxrtc_mode_survives_its_configuration(self):
        small = slim.slim(run("oxrtc", 1, 10.0))
        self.assertEqual(work.oxrtc_mode(small), work.LIVE)
        self.assertNotIn("oxrtc_mode", slim.slim(run("go2rtc", 1, 10.0)))

    def test_a_slim_session_gives_the_same_tables(self):
        with tempfile.TemporaryDirectory() as tmp:
            full, small = Path(tmp) / "full", Path(tmp) / "small"
            full.mkdir()
            for name, cpu in (("go2rtc", 20.0), ("oxrtc", 10.0), ("mediamtx", 30.0)):
                for rep in (1, 2, 3):
                    record = run(name, rep, cpu + rep)
                    (full / f"{record['run_id']}.json").write_text(json.dumps(record), encoding="utf-8")
            (full / "session.json").write_text("{}", encoding="utf-8")
            self.assertEqual(slim.main([str(full), "--out", str(small)]), 0)
            self.assertFalse((small / "session.json").exists())
            outputs = []
            for session in (full, small):
                md, csv = Path(tmp) / f"{session.name}.md", Path(tmp) / f"{session.name}.csv"
                tables.main([str(Path(tmp) / "none"), str(session), "--out-md", str(md), "--out-csv", str(csv)])
                outputs.append(csv.read_text(encoding="utf-8"))
            self.assertEqual(outputs[0], outputs[1])
            self.assertIn("live (webrtc.lip_sync: false)", outputs[1])
            self.assertIn("oxrtc better", outputs[1])
            self.assertLess(sum(p.stat().st_size for p in small.iterdir()),
                            sum(p.stat().st_size for p in full.iterdir()) / 5)


if __name__ == "__main__":
    unittest.main()
