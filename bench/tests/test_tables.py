"""bench/tables.py: a cell stays one Markdown cell whatever its reason says."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import tables  # noqa: E402


def row() -> tables.Row:
    return tables.Row(table="real", scenario="RC1", camera="camera_4", label="frames decoded per s",
                      unit="fps", lower=False, digits=1)


class CellText(unittest.TestCase):
    def test_a_reason_with_ffmpegs_stderr_stays_on_one_line(self):
        # go2rtc's error for a recorder whose input FFmpeg refused (RC1, 2026-11-19).
        reason = ("streams: wrong response on DESCRIBE, exec/rtsp\n[rtsp @ 0001] method DESCRIBE failed: "
                  "404Not Found\r\n[in#0 @ 0002] Error opening input | Server returned 404")
        r = row()
        r.per_run["go2rtc"] = [(1.0, None), (1.0, None), (None, reason)]
        text = tables.cell_text(r, "go2rtc", {})
        self.assertNotIn("\n", text)
        self.assertNotIn("\r", text)
        self.assertNotIn("|", text)
        self.assertIn("method DESCRIBE failed: 404Not Found [in#0 @ 0002]", text)
        self.assertIn("1 of 3 runs gave none", text)

    def test_a_failed_cell_stays_on_one_line(self):
        r = row()
        r.per_run["oxrtc"] = [(None, "first line\nsecond line")]
        self.assertEqual(tables.cell_text(r, "oxrtc", {}), "failed: first line second line (1 of 1 runs)")


class Scoreboard(unittest.TestCase):
    def test_the_tables_open_with_the_scoreboard_and_each_row_names_the_mode(self):
        won = tables.Row(table="synthetic", scenario="L01", camera="", label="latency", unit="ms", lower=True,
                         digits=1, mode="live (webrtc.lip_sync: false)")
        won.per_run = {"go2rtc": [(400.0, None), (410.0, None)], "oxrtc": [(10.0, None), (11.0, None)],
                       "mediamtx": [(100.0, None), (105.0, None)]}
        stress = tables.Row(table="synthetic", scenario="B05", camera="", label="freezes", unit="count",
                            lower=True, digits=1, mode="lip sync on (default)")
        stress.per_run = {"go2rtc": [(1.0, None), (1.1, None)], "oxrtc": [(9.0, None), (9.1, None)],
                          "mediamtx": [(3.0, None), (3.1, None)]}
        lines = tables.score_lines([won, stress], {})
        self.assertEqual(lines[0], "### Scoreboard (D29: oxrtc against go2rtc and MediaMTX)")
        self.assertIn("Counted rows (D29), 1: 1 won", lines[2])
        self.assertIn("2: 1 won, 0 tied, 0 tied behind, 1 lost", "".join(lines[4]))
        rendered = tables.render_table([won, stress], {})
        self.assertIn("#### L01", rendered)
        self.assertIn("#### B05 (stress rows: run and reported, outside the D29 count, D35)", rendered)
        head, records = tables.csv_records([won], {})
        self.assertEqual(head[-1], "oxrtc mode")
        self.assertEqual(records[0][-2:], ["oxrtc better", "live (webrtc.lip_sync: false)"])


def synthetic_run(sid: str, name: str, cpu: float, latency: float | None) -> dict:
    viewer = {"first_frame_ms": [300.0], "presented_fps": 25.0}
    if latency is not None:
        viewer["latency_ms_mean"] = latency
    return {"run_id": f"{sid}_{name}_r1_x", "status": "ok", "scenario": {"id": sid}, "server": {"name": name},
            "summary": {"cpu_pct_core_mean": cpu, "rss_mb_mean": 20.0}, "browser": {"viewers": [viewer]}}


class OtherSynthetic(unittest.TestCase):
    def test_every_synthetic_scenario_a_session_runs_has_rows(self):
        # B01 to B03, B07, L02 and L03 were run and published with no row (bench round 1).
        syn = tables.by_scenario([synthetic_run(sid, name, cpu, lat)
                                  for sid, lat in (("B01", 60.0), ("L02", None))
                                  for name, cpu in (("go2rtc", 3.0), ("oxrtc", 1.0))])
        rows = tables.rows_synthetic(syn)
        labels = {(r.scenario, r.label) for r in rows}
        self.assertIn(("B01", "glass-to-glass latency (mean)"), labels)
        self.assertIn(("B01", "server tree CPU"), labels)
        self.assertIn(("L02", "first frame"), labels)
        # A figure no run has is no row: L02 carries no frame code, so no latency.
        self.assertNotIn(("L02", "glass-to-glass latency (mean)"), labels)
        self.assertNotIn(("B01", "RTSP reader packets"), labels)
        cpu = next(r for r in rows if r.scenario == "B01" and r.label == "server tree CPU")
        self.assertEqual(cpu.per_run["oxrtc"], [(1.0, None)])


if __name__ == "__main__":
    unittest.main()
