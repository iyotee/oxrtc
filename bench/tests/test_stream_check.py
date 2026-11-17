"""stream_check.py: what the check concludes from a probe's result (roadmap W1)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import stream_check  # noqa: E402


def result(streams: list[list[str]], width: int = 1920) -> dict:
    kinds = ["video", "audio"]
    return {
        "tracks": [{"kind": kinds[i % 2], "track": f"t{i}", "streams": ids} for i, ids in enumerate(streams)],
        "video": {"videoWidth": width, "framesPresented": 100},
    }


class Verdict(unittest.TestCase):
    def test_one_stream_and_a_picture_pass(self):
        self.assertEqual(stream_check.verdict(result([["s"], ["s"]]), ["video", "audio"]), [])

    def test_a_stream_per_track_fails(self):
        problems = stream_check.verdict(result([["picture"], ["sound"]]), ["video", "audio"])
        self.assertEqual(len(problems), 1)
        self.assertIn("2 streams for 2 tracks", problems[0])

    def test_no_picture_fails_even_with_one_stream(self):
        problems = stream_check.verdict(result([["s"], ["s"]], width=0), ["video", "audio"])
        self.assertEqual(len(problems), 1)
        self.assertIn("no picture", problems[0])

    def test_a_sound_only_offer_needs_no_picture(self):
        self.assertEqual(stream_check.verdict(result([["s"]], width=0), ["audio"]), [])

    def test_a_track_with_no_stream_and_no_track_at_all_fail(self):
        self.assertIn("a track event carried no stream", stream_check.verdict(result([["s"], []]), ["video"]))
        self.assertIn("no track event", stream_check.verdict(result([]), ["video"]))

    def test_the_probes_own_errors_are_problems(self):
        failed = {"error": "probe wrote no output", "errors": ["answer 500: no"]}
        problems = stream_check.verdict(failed, ["video"])
        self.assertIn("probe: probe wrote no output", problems)
        self.assertIn("answer 500: no", problems)

    def test_the_first_stream_of_each_event_is_what_the_player_takes(self):
        self.assertEqual(stream_check.first_streams(result([["a", "b"], ["c"], []])), ["a", "c", None])


class JitterBuffer(unittest.TestCase):
    def reading(self, delay: float, target: float, emitted: int) -> list[dict]:
        return [{"kind": "video", "jitterBufferDelay": delay, "jitterBufferTargetDelay": target,
                 "jitterBufferEmittedCount": emitted}]

    def test_the_figure_is_per_unit_emitted_over_the_second_half(self):
        probe = {"statsHalf": self.reading(1.0, 2.0, 100), "statsEnd": self.reading(4.0, 3.0, 200)}
        self.assertAlmostEqual(stream_check.buffer_ms(probe, "video"), 30.0)
        self.assertAlmostEqual(stream_check.buffer_ms(probe, "video", "jitterBufferTargetDelay"), 10.0)

    def test_no_figure_without_the_kind_or_without_anything_emitted(self):
        probe = {"statsHalf": self.reading(1.0, 2.0, 100), "statsEnd": self.reading(4.0, 3.0, 100)}
        self.assertIsNone(stream_check.buffer_ms(probe, "video"))
        self.assertIsNone(stream_check.buffer_ms(probe, "audio"))
        self.assertIsNone(stream_check.buffer_ms({}, "video"))


class PictureHold(unittest.TestCase):
    def test_the_mean_the_p95_and_the_window(self):
        # 25 pictures a second (3600 ticks of 90 kHz), each held 10 ms, the last one 110 ms.
        frames = [[40 * i, 10.0, 3600 * i] for i in range(20)] + [[800, 110.0, 3600 * 20]]
        hold = stream_check.picture_hold({"frames": frames})
        self.assertEqual(hold["frames"], 21)
        self.assertAlmostEqual(hold["mean_ms"], (20 * 10 + 110) / 21)
        self.assertEqual(hold["p95_ms"], 10.0)
        self.assertAlmostEqual(hold["longest_stall_ms"], 0.0)
        late = stream_check.picture_hold({"frames": frames}, start_ms=790)
        self.assertEqual((late["frames"], late["mean_ms"]), (1, 110.0))
        self.assertEqual(stream_check.picture_hold({"frames": frames}, 0, 100)["frames"], 3)

    def test_a_stall_is_a_display_later_than_its_rtp_time_asks(self):
        # The third picture is shown 500 ms after the second, 40 ms of camera time later.
        frames = [[0, 5.0, 0], [40, 5.0, 3600], [540, 5.0, 7200], [580, 5.0, 10800]]
        self.assertAlmostEqual(stream_check.picture_hold({"frames": frames})["longest_stall_ms"], 460.0)

    def test_the_rtp_time_wraps_without_a_stall(self):
        top = (1 << 32) - 1800
        frames = [[0, 5.0, top], [40, 5.0, (top + 3600) % (1 << 32)]]
        self.assertAlmostEqual(stream_check.picture_hold({"frames": frames})["longest_stall_ms"], 0.0)

    def test_no_frame_no_figure(self):
        self.assertIsNone(stream_check.picture_hold({}))
        self.assertIsNone(stream_check.picture_hold({"frames": [[10, 1.0, 0]]}, start_ms=20))


if __name__ == "__main__":
    unittest.main()
