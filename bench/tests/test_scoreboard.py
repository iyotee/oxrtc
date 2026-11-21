"""bench/scoreboard.py: the counts of a session, pinned on the session of 2026-11-17."""

from __future__ import annotations

import io
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import scoreboard  # noqa: E402

#: openIVR's results_bench/results.csv of the session of 2026-11-17 (oxrtc 0.1.0,
#: go2rtc 1.9.15, MediaMTX 1.21.3), as published: 160 rows.
SESSION = Path(__file__).resolve().parent / "data" / "results-2026-11-17.csv"


def row(verdict: str, scenario: str = "RC1", oxrtc: str = "1.0") -> dict:
    return {"scenario": scenario, "metric": "m", "unit": "u", "verdict": verdict, "oxrtc mean": oxrtc,
            "go2rtc mean": "1.0", "MediaMTX mean": "1.0"}


class Classes(unittest.TestCase):
    def test_each_verdict_has_its_class(self):
        self.assertEqual(scoreboard.classify(row("oxrtc better")), "won")
        self.assertEqual(scoreboard.classify(row("no difference (go2rtc, oxrtc, MediaMTX)")), "tied")
        self.assertEqual(scoreboard.classify(row("no difference (oxrtc, MediaMTX)")), "tied")
        self.assertEqual(scoreboard.classify(row("no difference (go2rtc, MediaMTX)")), "tied behind")
        self.assertEqual(scoreboard.classify(row("go2rtc better")), "lost")
        self.assertEqual(scoreboard.classify(row("MediaMTX better")), "lost")
        self.assertEqual(scoreboard.classify(row("")), "without verdict")
        self.assertEqual(scoreboard.classify(row("n<2: no conclusion")), "without verdict")

    def test_a_row_oxrtc_has_no_value_of_is_no_verdict(self):
        # go2rtc and MediaMTX alone (oxrtc failed or did not run): nothing to count for oxrtc.
        self.assertEqual(scoreboard.classify(row("go2rtc better", oxrtc="")), "without verdict")
        self.assertEqual(scoreboard.classify(row("no difference (go2rtc, MediaMTX)", oxrtc="")), "without verdict")

    def test_the_stress_rows_are_counted_apart(self):
        rows = [row("go2rtc better", "B05"), row("oxrtc better", "B06"), row("oxrtc better", "RC1")]
        lines = scoreboard.board(rows)
        self.assertIn("**Counted rows (D29), 1: 1 won, 0 tied, 0 tied behind, 0 lost, 0 without verdict**",
                      lines[2])
        self.assertIn("Stress rows (B05, B06; D35, outside the count), 2: 1 won, 0 tied, 0 tied behind, 1 lost",
                      lines[3])
        lost = [line for line in lines if line.startswith("| B05")]
        self.assertEqual(len(lost), 1)
        self.assertIn("(stress)", lost[0])

    def test_a_listed_row_names_the_oxrtc_mode(self):
        listed = row("go2rtc better")
        listed["oxrtc mode"] = "live (webrtc.lip_sync: false)"
        line = scoreboard.detail_lines([listed], "lost")[-1]
        self.assertTrue(line.endswith("| live (webrtc.lip_sync: false) |"), line)
        self.assertTrue(scoreboard.detail_lines([row("go2rtc better")], "lost")[-1].endswith("| not recorded |"))


class SessionOf20261117(unittest.TestCase):
    def test_the_session_reads_as_d29_says(self):
        rows = scoreboard.read(SESSION)
        self.assertEqual(len(rows), 160)
        tally = scoreboard.counts(rows)
        self.assertEqual([tally[c] for c in scoreboard.CLASSES], [28, 98, 6, 15, 13])

    def test_the_d29_count_leaves_b05_and_b06_out(self):
        rows = scoreboard.read(SESSION)
        counted = scoreboard.counts([r for r in rows if not scoreboard.is_stress(r)])
        stress = scoreboard.counts([r for r in rows if scoreboard.is_stress(r)])
        self.assertEqual([counted[c] for c in scoreboard.CLASSES], [20, 92, 5, 14, 11])
        self.assertEqual([stress[c] for c in scoreboard.CLASSES], [8, 6, 1, 1, 2])

    def test_the_command_prints_the_counts_and_every_lost_and_tied_behind_row(self):
        out = io.StringIO()
        with redirect_stdout(out):
            self.assertEqual(scoreboard.main([str(SESSION)]), 0)
        text = out.getvalue()
        self.assertIn("All rows, 160: 28 won, 98 tied, 6 tied behind, 15 lost, 13 without verdict", text)
        listed = [line for line in text.splitlines() if line.startswith("| R") or line.startswith("| B")]
        self.assertEqual(len(listed), 15 + 6)
        self.assertIn("| RC1 | its FFmpeg children's CPU | % of one core | 234.3 ± 12.7 | 218.5 ± 28.1 | "
                      "126.8 ± 15.2 | MediaMTX better | not recorded |", text)


if __name__ == "__main__":
    unittest.main()
