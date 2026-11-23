"""bench/peers.py: live oxrtc against go2rtc, default oxrtc against MediaMTX, each pair in its session."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import peers  # noqa: E402
import tables  # noqa: E402
from tables import Row  # noqa: E402


def row(go2rtc, oxrtc, mediamtx, scenario="L01", label="glass-to-glass latency (mean)", lower=True) -> Row:
    r = Row("synthetic", scenario, "", label, "ms", lower, 1)
    r.per_run = {"go2rtc": [(v, None) for v in go2rtc], "oxrtc": [(v, None) for v in oxrtc],
                 "mediamtx": [(v, None) for v in mediamtx]}
    return r


class Pairs(unittest.TestCase):
    def test_each_mode_is_judged_against_its_peer_from_its_own_session(self):
        default = row([480, 490, 470], [80, 82, 78], [115, 117, 113])
        live = row([300, 310, 290], [8, 9, 7], [116, 114, 115])
        (vs_go, vs_mtx, base), = peers.peer_rows([default], [live])
        self.assertIs(base, default)
        # against go2rtc: the live session's go2rtc and the live oxrtc, never the default's
        self.assertEqual(tables.interval(vs_go, "go2rtc").mean, 300)
        self.assertEqual(tables.interval(vs_go, "oxrtc").mean, 8)
        self.assertIsNone(tables.interval(vs_go, "mediamtx"))
        # against MediaMTX: the default session's MediaMTX and the default oxrtc
        self.assertEqual(tables.interval(vs_mtx, "oxrtc").mean, 80)
        self.assertEqual(tables.interval(vs_mtx, "mediamtx").mean, 115)
        self.assertIsNone(tables.interval(vs_mtx, "go2rtc"))
        self.assertEqual(tables.verdict(vs_go)[0], "oxrtc better")
        self.assertEqual(tables.verdict(vs_mtx)[0], "oxrtc better")

    def test_a_tie_with_the_peer_says_no_difference(self):
        default = row([480, 490, 470], [100, 130, 110], [115, 117, 113])
        (_, vs_mtx, _), = peers.peer_rows([default], [row([1, 2, 3], [1, 2, 3], [1, 2, 3])])
        self.assertTrue(tables.verdict(vs_mtx)[0].startswith("no difference"))

    def test_rows_the_live_sessions_did_not_run_and_information_rows_are_left_out(self):
        default = [row([1, 2], [1, 2], [1, 2], scenario="B01"), row([1, 2], [1, 2], [1, 2], lower=None)]
        live = [row([1, 2], [1, 2], [1, 2], lower=None)]
        self.assertEqual(peers.peer_rows(default, live), [])

    def test_the_table_names_both_modes(self):
        triples = peers.peer_rows([row([480, 490], [80, 82], [115, 117])], [row([300, 310], [8, 9], [1, 2])])
        text = "\n".join(peers.render(triples))
        self.assertIn("oxrtc live", text)
        self.assertIn("oxrtc default", text)
        self.assertIn("| L01 | glass-to-glass latency (mean) |", text)


if __name__ == "__main__":
    unittest.main()
