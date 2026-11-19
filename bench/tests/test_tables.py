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


if __name__ == "__main__":
    unittest.main()
