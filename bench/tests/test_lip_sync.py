"""Lip sync as the L01 probe reports it (clients/browser.py): the browser's
own figures by the servers' dates (`lip_sync`), and the beep heard against the
flash seen (`av_offsets`), which no server's dating can move.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from clients.browser import av_offsets, lip_sync  # noqa: E402

FRAME_MS = 40.0  # 25 pictures a second: a flash every 25 codes


def frames(count: int, start_ms: float = 1000.0, first_code: int = 0) -> list:
    """Presented frames [wall, code, presented], one per 40 ms."""
    return [[start_ms + i * FRAME_MS, (first_code + i) % 4096, i] for i in range(count)]


class AvOffsets(unittest.TestCase):
    def test_a_beep_heard_after_its_flash_is_a_positive_offset(self):
        shown = frames(250)  # 10 s, flashes at 1000, 2000 ... 10000 ms
        beeps = [1000 + 1000 * k + 80 for k in range(10)]
        out = av_offsets(shown, beeps, FRAME_MS)
        self.assertEqual(out["av_pairs"], 10)
        self.assertAlmostEqual(out["av_offset_ms"], 80.0)

    def test_a_beep_heard_before_its_flash_is_negative_and_the_worst_keeps_its_sign(self):
        shown = frames(250)
        beeps = [1000 + 1000 * k - 240 for k in range(1, 10)]
        beeps[3] -= 30
        out = av_offsets(shown, beeps, FRAME_MS)
        self.assertEqual(out["av_pairs"], 9)
        self.assertAlmostEqual(out["av_offset_worst_ms"], -270.0)

    def test_only_whole_seconds_of_frames_are_flashes(self):
        # The codes start at 7: the flashes are codes 25, 50 ... (wall 1720, 2720 ...).
        shown = frames(250, first_code=7)
        out = av_offsets(shown, [1720 + 50.0, 2720 + 50.0], FRAME_MS)
        self.assertEqual(out["av_pairs"], 2)
        self.assertAlmostEqual(out["av_offset_ms"], 50.0)

    def test_a_beep_with_no_flash_within_400_ms_is_not_paired(self):
        shown = frames(30)  # one flash, at 1000 ms
        self.assertEqual(av_offsets(shown, [1000 + 450.0, 3000.0], FRAME_MS), {"av_pairs": 0})

    def test_the_pairs_before_the_start_are_left_out(self):
        shown = frames(250)
        beeps = [1000 + 1000 * k + (300 if k < 5 else 20) for k in range(10)]
        out = av_offsets(shown, beeps, FRAME_MS, start_ms=5500)
        self.assertEqual(out["av_pairs"], 5)
        self.assertAlmostEqual(out["av_offset_ms"], 20.0)

    def test_nothing_to_pair(self):
        self.assertEqual(av_offsets([], [1000.0], FRAME_MS), {"av_pairs": 0})
        self.assertEqual(av_offsets(frames(50), [], FRAME_MS), {"av_pairs": 0})


def reading(wall: float, kind: str, delay_s: float, emitted: int, playout: float | None = None, **extra) -> dict:
    out = {"wall": wall, "kind": kind, "jitterBufferDelay": delay_s, "jitterBufferEmittedCount": emitted, **extra}
    if playout is not None:
        out["estimatedPlayoutTimestamp"] = playout
    return out


class LipSyncFromStats(unittest.TestCase):
    def test_the_buffers_per_unit_and_the_lag_over_the_second_half(self):
        stats = []
        for second in range(10):
            wall = 1000.0 * second
            # 60 ms of sound per frame, 90 ms of picture; the picture 25 ms ahead.
            stats.append(reading(wall, "audio", 0.06 * 50 * second, 50 * second, playout=wall,
                                 concealedSamples=80, totalSamplesReceived=8000))
            stats.append(reading(wall, "video", 0.09 * 25 * second, 25 * second, playout=wall + 25,
                                 jitterBufferTargetDelay=0.08 * 25 * second,
                                 jitterBufferMinimumDelay=0.01 * 25 * second))
        out = lip_sync(stats)
        self.assertAlmostEqual(out["sound_held_ms"], 60.0)
        self.assertAlmostEqual(out["picture_held_ms"], 90.0)
        self.assertAlmostEqual(out["picture_asked_ms"], 70.0)
        self.assertAlmostEqual(out["sound_behind_ms"], 25.0)
        self.assertAlmostEqual(out["sound_concealed_pct"], 1.0)

    def test_no_readings(self):
        self.assertEqual(lip_sync([]), {})


if __name__ == "__main__":
    unittest.main()
