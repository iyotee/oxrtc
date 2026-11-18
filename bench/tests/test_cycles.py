"""The spread of the cycling viewers' offers over the camera's GOP (L02, L03).

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import scenario  # noqa: E402
from clients.browser import Viewer, cycle_delays  # noqa: E402


class CycleDelays(unittest.TestCase):
    def test_none_without_cycles_or_spread(self):
        self.assertEqual(cycle_delays(0, 4830), [])
        self.assertEqual(cycle_delays(20, 0), [])

    def test_the_same_sequence_every_time(self):
        self.assertEqual(cycle_delays(20, 4830), cycle_delays(20, 4830))

    def test_every_quarter_of_the_gop_is_sampled(self):
        # 20 offers over one GOP: each quarter gets about five. A phase-locked
        # schedule would put all of them in one.
        delays = cycle_delays(20, 4830)
        self.assertTrue(all(0 <= d < 4830 for d in delays), delays)
        quarters = [sum(1 for d in delays if q * 4830 / 4 <= d < (q + 1) * 4830 / 4) for q in range(4)]
        self.assertTrue(all(4 <= n <= 6 for n in quarters), quarters)

    def test_the_job_carries_one_delay_per_cycle(self):
        viewer = Viewer("v", "http://x", {}, 1920, cycles=5, spread_ms=1000)
        self.assertEqual(viewer.job()["cycleDelaysMs"], cycle_delays(5, 1000))
        self.assertEqual(len(viewer.job()["cycleDelaysMs"]), 5)

    def test_l02_and_l03_spread_over_one_gop(self):
        for sid in ("L02", "L03"):
            data = scenario.load(sid)
            profile = scenario.profile(data)
            fps_num, _, fps_den = str(profile.fps).partition("/")
            gop_ms = profile.gop * 1000 * int(fps_den or 1) / int(fps_num)
            (group,) = data["webrtc"]
            self.assertAlmostEqual(group["spread_ms"], gop_ms, delta=1, msg=sid)



class Reconnect(unittest.TestCase):
    def test_off_unless_asked(self):
        self.assertFalse(Viewer("v", "http://x", {}, 1920).job()["reconnect"])
        self.assertTrue(Viewer("v", "http://x", {}, 1920, reconnect=True).job()["reconnect"])

    def test_the_recovery_scenarios_reconnect(self):
        # MediaMTX closes its viewers' sessions when the source drops: without
        # a reconnecting viewer its recovery is never measured.
        for sid in ("R01", "R02"):
            (group,) = scenario.load(sid)["webrtc"]
            self.assertIs(group.get("reconnect"), True, sid)

    def test_a_reconnection_is_not_a_first_frame(self):
        from clients.browser import analyze_viewer

        viewer = {"label": "v", "frames": [], "attempts": [
            {"firstFrameMs": 300.0, "reconnect": False},
            {"firstFrameMs": None, "reconnect": True, "status": 404},
            {"firstFrameMs": 140.0, "reconnect": True},
        ]}
        out = analyze_viewer(viewer, None, 40.0)
        self.assertEqual(out["first_frame_ms"], [300.0])
        self.assertEqual(out["failed_attempts"], 0)
        self.assertEqual(out["reconnect_attempts"], 2)

    def test_a_viewer_keeps_the_counters_that_decide_smoothness(self):
        # W3: frames received against frames decoded is what showed a server
        # sending more than the camera did (its replays), and the keyframe
        # requests, retransmissions and inter-frame spread say why.
        from clients.browser import analyze_viewer

        last = {"kind": "video", "wall": 2000.0, "framesReceived": 4566, "framesDecoded": 1895,
                "keyFramesDecoded": 110, "pliCount": 160, "firCount": 0, "retransmittedPacketsReceived": 0,
                "totalInterFrameDelay": 119.7, "totalSquaredInterFrameDelay": 25.1, "totalProcessingDelay": 263.7}
        out = analyze_viewer({"label": "v", "frames": [], "stats": [last]}, None, 40.0)
        kept = out["video_stats_last"]
        for key in ("framesReceived", "keyFramesDecoded", "firCount", "retransmittedPacketsReceived",
                    "totalInterFrameDelay", "totalSquaredInterFrameDelay", "totalProcessingDelay"):
            self.assertEqual(kept[key], last[key], key)


if __name__ == "__main__":
    unittest.main()
