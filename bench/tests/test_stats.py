"""Confidence intervals, the report's verdicts, the run order and the scenario files.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import math
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import report  # noqa: E402
import scenario  # noqa: E402
from run import schedule  # noqa: E402
from stats import Interval, fmt, mean_ci, t975  # noqa: E402


class ConfidenceInterval(unittest.TestCase):
    def test_known_values(self):
        # mean 5, sample sd = sqrt(2.5), n = 5: half = 2.776 * sqrt(2.5 / 5)
        ci = mean_ci([3, 4, 5, 6, 7])
        self.assertAlmostEqual(ci.mean, 5.0)
        self.assertAlmostEqual(ci.half_width, 2.776 * math.sqrt(0.5), places=6)
        self.assertAlmostEqual(ci.low, 5.0 - ci.half_width)

    def test_three_runs_use_two_degrees_of_freedom(self):
        ci = mean_ci([10.0, 12.0, 14.0])
        self.assertAlmostEqual(ci.half_width, 4.303 * 2.0 / math.sqrt(3), places=6)

    def test_identical_runs_have_zero_width(self):
        self.assertEqual(mean_ci([2.0, 2.0, 2.0]).half_width, 0.0)

    def test_one_run_has_no_interval(self):
        ci = mean_ci([42.0])
        self.assertEqual((ci.n, ci.mean, ci.half_width), (1, 42.0, None))
        self.assertIn("no CI", fmt(ci))

    def test_empty_and_missing_values(self):
        self.assertIsNone(mean_ci([]))
        self.assertEqual(mean_ci([1.0, None, float("nan"), 3.0]).n, 2)  # type: ignore[list-item]
        self.assertEqual(fmt(None), "-")

    def test_t_table_is_conservative_between_rows(self):
        self.assertEqual(t975(2), 4.303)
        self.assertEqual(t975(35), t975(30))
        self.assertEqual(t975(1000), 1.960)
        with self.assertRaises(ValueError):
            t975(0)


class Verdicts(unittest.TestCase):
    def test_overlap_is_no_difference(self):
        a, b = Interval(3, 10.0, 2.0), Interval(3, 11.0, 2.0)
        self.assertTrue(a.overlaps(b))
        self.assertEqual(report.verdict({"a": a, "b": b}, True), "no difference")

    def test_separated_intervals_name_the_better(self):
        a, b = Interval(3, 10.0, 1.0), Interval(3, 20.0, 1.0)
        self.assertEqual(report.verdict({"a": a, "b": b}, True), "a better")
        self.assertEqual(report.verdict({"a": a, "b": b}, False), "b better")

    def test_single_runs_conclude_nothing(self):
        a, b = Interval(1, 10.0, None), Interval(1, 20.0, None)
        self.assertEqual(report.verdict({"a": a, "b": b}, True), "n<2: no conclusion")

    def test_report_never_uses_a_median(self):
        text = Path(report.__file__).read_text(encoding="utf-8")
        code = text.split('"""', 2)[2]  # past the module docstring
        self.assertNotIn("median(", code)


class RunOrder(unittest.TestCase):
    def test_contenders_are_interleaved(self):
        self.assertEqual(schedule(["a", "b"], 3, "abab"),
                         [(0, "a"), (0, "b"), (1, "a"), (1, "b"), (2, "a"), (2, "b")])

    def test_abba(self):
        self.assertEqual([n for _, n in schedule(["a", "b"], 4, "abba")], list("abbaabba"))


class Faults(unittest.TestCase):
    """The fault thread never restarts a publisher outside the measured window."""

    def _run(self, duration):
        import tempfile

        from run import Run
        from servers.go2rtc import Go2rtc

        tmp = tempfile.mkdtemp()
        return Run(scenario.load("R01"), Go2rtc, 0, Path(tmp), duration)

    def test_faults_past_a_short_window_are_skipped_not_played(self):
        run = self._run(20)  # R01's faults are at 15 s + 10 s and 50 s + 60 s
        run._faults(time.time(), None, None)  # no publisher exists: touching one would raise
        self.assertEqual([e.get("skipped") for e in run.events], ["ends after the measured window"] * 2)

    def test_a_halted_run_restarts_nothing(self):
        run = self._run(None)
        run._halt.set()
        began = time.monotonic()
        run._faults(time.time(), None, None)
        self.assertLess(time.monotonic() - began, 1.0)
        self.assertEqual(run.events, [])


class Scenarios(unittest.TestCase):
    def test_every_benchmarks_id_has_a_valid_file(self):
        ids = {"B01", "B02", "B03", "B04", "B05", "B06", "B07", "L01", "L02", "L03",
               "R01", "R02", "R03", "S01", "X01", "RC1", "RC2", "RC3", "RC4"}
        self.assertEqual(set(scenario.all_ids()), ids)
        for sid in ids:
            scenario.load(sid)

    def test_validation_rejects_bad_files(self):
        base = {"id": "Z", "title": "t", "measures": [], "status": "ready", "duration_s": 10,
                "cameras": {"count": 1, "profile": {}}}
        scenario.validate(dict(base))
        for bad in ({"status": "later"}, {"cameras": {"count": 0}}, {"duration_s": 0},
                    {"webrtc": [{"want": "vp9"}]}, {"webrtc": [{"cameras": [2]}]},
                    {"cameras": {"count": 1, "profile": {"codec_x": 1}}}, {"surprise": 1},
                    {"webrtc": [{"per_camera": 300}]}):
            with self.assertRaises(scenario.ScenarioError, msg=str(bad)):
                scenario.validate({**base, **bad})
        with self.assertRaises(scenario.ScenarioError):
            scenario.validate({**base, "status": "needs-linux"})  # no reason given


if __name__ == "__main__":
    unittest.main()
