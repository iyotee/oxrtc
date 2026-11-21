"""bench/work.py: what a contender did, beside what it cost (W6)."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import work  # noqa: E402


class Mode(unittest.TestCase):
    def test_the_mode_is_read_from_the_recorded_configuration(self):
        config = "webrtc:\n  listen: :1\n  max_sessions: 256\n"
        self.assertEqual(work.oxrtc_mode({"server_config": config}), work.LIP_SYNC)
        live = config + "  lip_sync: false\n"
        self.assertEqual(work.oxrtc_mode({"server_config": live}), work.LIVE)
        lag = config + "  sound_lag_ms: 0\n"
        self.assertEqual(work.oxrtc_mode({"server_config": lag}), f"{work.LIP_SYNC}, sound_lag_ms 0")

    def test_a_slim_run_carries_its_mode_and_an_unknown_one_says_so(self):
        self.assertEqual(work.oxrtc_mode({"oxrtc_mode": work.LIVE}), work.LIVE)
        self.assertEqual(work.oxrtc_mode({}), work.NOT_RECORDED)

    def test_oxrtc_0_1_is_a_stream_per_track(self):
        run = {"server": {"version": "oxrtc version 0.1.0 windows/amd64"}, "server_config": "webrtc: {}\n"}
        self.assertEqual(work.oxrtc_mode(run), work.STREAM_PER_TRACK)
        run["server"]["version"] = "oxrtc version 10.1.0 windows/amd64"
        self.assertEqual(work.oxrtc_mode(run), work.LIP_SYNC)


if __name__ == "__main__":
    unittest.main()
