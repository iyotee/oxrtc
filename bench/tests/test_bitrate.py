"""The transcode rows at equal bitrate (bitrate.py, the owner's answer of 2026-11-24, D29).

Every contender's H.264 transcode of a camera is held to the same rate: go2rtc's
and oxrtc's through go2rtc's `#bitrate=`, MediaMTX's FFmpeg through the three
arguments that fragment writes. `OXRTC_BENCH_BITRATE=defaults` imposes none.

Run: python -m pytest -q bench/tests   (from the repository root)
"""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import bitrate  # noqa: E402
from servers import go2rtc, mediamtx, oxrtc  # noqa: E402
from servers.base import Camera  # noqa: E402

PLAIN = [Camera(1, "rtsp://cam/1", "rtsp://cam/1s"), Camera(2, "rtsp://cam/2", "rtsp://cam/2s")]
REAL = [Camera(5, "http://webcam/5", ""), Camera(11, "http://webcam/11", "")]


class Values(unittest.TestCase):
    def test_real_cameras_get_their_own_rate_and_an_unknown_one_the_fallback(self):
        with mock.patch.dict(os.environ, {bitrate.ENV: ""}):
            self.assertEqual(bitrate.real([5, 11, 99]), {5: 1800, 11: 4000, 99: bitrate.REAL_OTHER_KBPS})

    def test_synthetic_cameras_share_their_profile_rate(self):
        with mock.patch.dict(os.environ, {bitrate.ENV: ""}):
            self.assertEqual(bitrate.synthetic(2, 2304, 1296, "100/7"), {1: 7300, 2: 7300})
            self.assertEqual(bitrate.synthetic(1, 640, 480, "10"), {1: bitrate.SYNTHETIC_OTHER_KBPS})

    def test_defaults_imposes_nothing(self):
        with mock.patch.dict(os.environ, {bitrate.ENV: "defaults"}):
            self.assertIsNone(bitrate.real([5]))
            self.assertIsNone(bitrate.synthetic(1, 1920, 1080, "25"))
        self.assertEqual(bitrate.fragment(None), "")
        self.assertEqual(bitrate.ffmpeg_args(None), [])

    def test_no_rate_is_below_what_oxrtc_sent_at_its_defaults_in_round_1(self):
        # oxrtc's mean video kbit/s per camera in the session of 2026-11-23 (RC1, RC4,
        # RC5; B02, B03, B07): the bench brings the others up to it, never oxrtc down.
        round_1 = {4: 1353, 5: 1704, 11: 3993, 12: 2319, 1: 6290, 2: 4506}
        for cid, sent in round_1.items():
            self.assertGreaterEqual(bitrate.REAL_KBPS[cid], sent, cid)
        self.assertGreaterEqual(bitrate.SYNTHETIC_KBPS[(1920, 1080, "25")], 8980)
        self.assertGreaterEqual(bitrate.SYNTHETIC_KBPS[(2304, 1296, "100/7")], 7262)


class Adapters(unittest.TestCase):
    def test_go2rtc_and_oxrtc_write_the_same_bitrate_fragment(self):
        kbps = {1: 9000, 2: 9000}
        for cls in (go2rtc.Go2rtc, oxrtc.Oxrtc):
            plain = cls(Path("unused"), PLAIN, transcode_kbps=kbps).streams({"copy", "h264"})
            self.assertEqual(plain["camera_1_h264"], ["ffmpeg:camera_1#video=h264#bitrate=9000k"], cls.name)
            self.assertEqual(plain["camera_1"], ["rtsp://cam/1#timeout=15"], "a copy has no rate")
            openivr = cls(Path("unused"), PLAIN, layout="openivr", transcode_kbps=kbps).streams(set())
            self.assertIn("#video=h264/live#bitrate=9000k#timeout=20", openivr["camera_2"][1], cls.name)
            self.assertNotIn("bitrate", " ".join(openivr["camera_2_rec"]), "the recorder's sound is no video")
            real = cls(Path("unused"), REAL, layout="real", transcode_kbps={5: 1800, 11: 4000}).streams(set())
            self.assertIn("#video=h264/live#bitrate=1800k#timeout=", real["camera_5"][1])
            self.assertIn("#video=h264/live#bitrate=4000k#timeout=", real["camera_11"][1])

    def test_mediamtx_ffmpeg_gets_the_three_arguments_bitrate_writes(self):
        server = mediamtx.Mediamtx(Path("unused"), PLAIN, transcode_kbps={1: 9000, 2: 9000})
        line = server.paths({"copy", "h264"})["camera_1_h264"]["runOnDemand"]
        self.assertIn(" -b:v 9000k -maxrate 9000k -bufsize 9000k -f rtsp ", line)
        real = mediamtx.Mediamtx(Path("unused"), REAL, layout="real", transcode_kbps={5: 1800, 11: 4000})
        line = real.real_paths()["camera_11"]["runOnDemand"]
        self.assertIn(" -b:v 4000k -maxrate 4000k -bufsize 4000k -map 0:a? ", line)
        self.assertLess(line.index("-fps_mode"), line.index("-b:v"), "after the live template's video codec")

    def test_without_a_rate_the_lines_are_as_before(self):
        for cls in (go2rtc.Go2rtc, oxrtc.Oxrtc):
            plain = cls(Path("unused"), PLAIN).streams({"copy", "h264"})
            self.assertEqual(plain["camera_1_h264"], ["ffmpeg:camera_1#video=h264"])
            openivr = cls(Path("unused"), PLAIN, layout="openivr").streams(set())
            self.assertIn("#video=h264/live#timeout=20", openivr["camera_1"][1])
        line = mediamtx.Mediamtx(Path("unused"), PLAIN).paths({"copy", "h264"})["camera_1_h264"]["runOnDemand"]
        self.assertNotIn("-b:v", line)

    def test_the_run_records_the_rate(self):
        server = oxrtc.Oxrtc(Path("unused"), REAL, layout="real", transcode_kbps={5: 1800, 11: 4000})
        self.assertEqual(server.describe()["transcode_kbps"], {"11": 4000, "5": 1800})
        self.assertIsNone(oxrtc.Oxrtc(Path("unused"), REAL).describe()["transcode_kbps"])


if __name__ == "__main__":
    unittest.main()
