"""openIVR's stream layout in the bench (B07): the streams, the scenario keys,
the recorder's hold, and the camera's 16 kHz sound.

The layout is a copy of what openIVR's go2rtc_manager.py writes. When the
openIVR repository sits next to this one, a contract test reads its source and
fails on drift; without it, that test is skipped.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import scenario  # noqa: E402
from cams import media  # noqa: E402
from clients.preload import Preload  # noqa: E402
from servers import go2rtc, mediamtx, oxrtc  # noqa: E402
from servers.base import Camera  # noqa: E402

OPENIVR = Path(__file__).resolve().parents[3] / "openIVR" / "app" / "services" / "streaming"
PORT = 40000


class Streams(unittest.TestCase):
    def test_four_streams_per_camera(self):
        streams = go2rtc.openivr_streams(3, "rtsp://cam/main", "rtsp://cam/sub", PORT)
        self.assertEqual(sorted(streams), ["camera_3", "camera_3_rec", "camera_3_src", "camera_3_sub"])
        video = f"rtsp://127.0.0.1:{PORT}/camera_3_src?video#timeout=20"
        self.assertEqual(streams["camera_3_src"], ["rtsp://cam/main#timeout=15"])
        self.assertEqual(streams["camera_3_rec"][0], video, "the recorder reads the relayed video only")
        self.assertIn("#audio=opus", streams["camera_3_rec"][1])
        self.assertEqual(streams["camera_3"][0], video)
        self.assertIn("#video=h264#width=1920#height=1080", streams["camera_3"][1])
        self.assertEqual(streams["camera_3"][2], f"rtsp://127.0.0.1:{PORT}/camera_3_rec?audio=opus#timeout=20",
                         "the viewer's sound comes last, from the recording stream")

    def test_the_adapters_that_can_and_cannot(self):
        cams = [Camera(1, "rtsp://cam/1", "rtsp://cam/1s"), Camera(2, "rtsp://cam/2", "rtsp://cam/2s")]
        for cls in (go2rtc.Go2rtc, oxrtc.Oxrtc):
            server = cls(Path("unused"), cams, layout="openivr")
            self.assertEqual(len(server.streams(set())), 8, cls.name)
            self.assertIn("camera_2_rec", server.config({"copy"})["streams"])
        self.assertNotIn("openivr", mediamtx.Mediamtx.layouts)
        plain = go2rtc.Go2rtc(Path("unused"), cams)
        self.assertNotIn("camera_1_rec", plain.streams({"copy"}), "the other scenarios keep their layout")


@unittest.skipUnless(OPENIVR.is_dir(), "the openIVR repository is not next to this one")
class OpenivrContract(unittest.TestCase):
    """What openivr_streams copies, read from openIVR's source."""

    def setUp(self):
        # openIVR renamed go2rtc_manager.py to media_server_manager.py when it switched to oxrtc
        name = "media_server_manager.py" if (OPENIVR / "media_server_manager.py").is_file() else "go2rtc_manager.py"
        self.manager = (OPENIVR / name).read_text(encoding="utf-8")
        self.patience = (OPENIVR / "patience.py").read_text(encoding="utf-8")

    def constant(self, text, name):
        found = re.search(rf"^{name}\s*=\s*(\d+)", text, re.M)
        self.assertIsNotNone(found, name)
        return int(found.group(1))

    def test_stall_timeouts(self):
        self.assertEqual(self.constant(self.patience, "CAMERA_STALL_SECONDS"), go2rtc.CAMERA_STALL_SECONDS)
        self.assertEqual(self.constant(self.patience, "RELAY_STALL_SECONDS"), go2rtc.RELAY_STALL_SECONDS)

    def test_the_sources(self):
        for fragment in (
            '{SOURCE_SUFFIX}?video"',  # the relayed video, alone
            "#video=h264#width=1920#height=1080",  # the viewer's transcode
            "#audio={AUDIO_CODEC}#timeout={RELAY_STALL_SECONDS}",  # the recorder's sound
            "{RECORDING_SUFFIX}?audio={AUDIO_CODEC}",  # the viewer's sound
        ):
            self.assertIn(fragment, self.manager)
        self.assertIn('AUDIO_CODEC = "opus"', self.manager)
        self.assertIn('RECORDING_SUFFIX = "_rec"', self.manager)
        self.assertIn('SOURCE_SUFFIX = "_src"', self.manager)


class Scenario(unittest.TestCase):
    def test_b07_is_valid(self):
        data = scenario.load("B07")
        self.assertEqual(scenario.layout(data), "openivr")
        self.assertEqual(scenario.profile(data).audio, "pcma16")

    def test_bad_keys_are_refused(self):
        base = {"id": "Z", "title": "t", "status": "ready", "measures": [], "duration_s": 10,
                "cameras": {"count": 1}}
        for bad in ({"layout": "frigate"}, {"rtsp": [{"suffix": "_x"}]},
                    {"preloads": [{"queries": "audio=opus"}]}, {"cameras": {"count": 1, "profile": {"audio": "aac"}}}):
            with self.assertRaises(scenario.ScenarioError, msg=bad):
                scenario.validate({**base, **bad})
        self.assertEqual(scenario.layout(base), "plain")


class FakeServer:
    """Answers 500 to audio=opus, as for a camera without a microphone."""

    def __init__(self):
        self.asked = []

    def supports_preload_api(self):
        return True

    def preload(self, stream, query=""):
        self.asked.append((stream, query))
        return 500 if query == "audio=opus" else 200


class Hold(unittest.TestCase):
    def test_the_sound_then_the_video(self):
        server = FakeServer()
        held = Preload("cam1", "camera_1_rec", Path("unused"), queries=["audio=opus", "video="]).start(server)
        self.assertEqual(server.asked, [("camera_1_rec", "audio=opus"), ("camera_1_rec", "video=")])
        self.assertEqual(held.result["status"], 200)
        self.assertEqual([a["status"] for a in held.result["answers"]], [500, 200])

    def test_one_query_by_default(self):
        server = FakeServer()
        Preload("cam1", "camera_1", Path("unused")).start(server)
        self.assertEqual(server.asked, [("camera_1", "")])


class Sound(unittest.TestCase):
    def test_sixteen_kilohertz_in_20_ms_packets(self):
        argv = media.build_argv(media.Profile(audio="pcma16"), "ffmpeg", Path("out.mkv"))
        text = " ".join(argv)
        self.assertIn("s=16000", text)
        self.assertIn("asetnsamples=n=320", text)
        self.assertEqual(argv[argv.index("-ar") + 1], "16000")

    def test_eight_kilohertz_unchanged(self):
        argv = media.build_argv(media.Profile(), "ffmpeg", Path("out.mkv"))
        self.assertIn("asetnsamples=n=160", " ".join(argv))
        self.assertEqual(argv[argv.index("-ar") + 1], "8000")

    def test_no_sound(self):
        argv = media.build_argv(media.Profile(audio="none"), "ffmpeg", Path("out.mkv"))
        self.assertNotIn("-ar", argv)



class Problems(unittest.TestCase):
    def test_every_viewer_must_see_a_frame(self):
        from run import viewer_problems

        self.assertEqual(viewer_problems([{"label": "a", "frames_presented": 10}]), [])
        found = viewer_problems([{"label": "a", "frames_presented": 10}, {"label": "b", "frames_presented": 0}])
        self.assertEqual(found, ["1 of 2 viewers presented no frame: b"])


class ReaderLabels(unittest.TestCase):
    def test_one_label_per_stream_read(self):
        from run import reader_label

        labels = {reader_label(1, False, "_rec", 0), reader_label(1, True, "", 0), reader_label(1, False, "", 0)}
        self.assertEqual(labels, {"cam1-rec-1", "cam1-sub-1", "cam1-1"})
        self.assertEqual(reader_label(1, False, "", 0), "cam1-1", "the plain scenarios keep their labels")


class LipSync(unittest.TestCase):
    def test_lip_sync_is_left_to_oxrtc_unless_the_run_turns_it_off(self):
        import os
        from unittest import mock

        cams = [Camera(1, "rtsp://cam/main", "rtsp://cam/sub")]
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(oxrtc.LIP_SYNC_ENV, None)
            self.assertNotIn("lip_sync", oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["webrtc"])
        with mock.patch.dict(os.environ, {oxrtc.LIP_SYNC_ENV: "0"}):
            self.assertIs(oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["webrtc"]["lip_sync"], False)

    def test_the_sound_lag_is_left_to_oxrtc_unless_the_run_sets_it(self):
        import os
        from unittest import mock

        cams = [Camera(1, "rtsp://cam/main", "rtsp://cam/sub")]
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(oxrtc.SOUND_LAG_ENV, None)
            self.assertNotIn("sound_lag_ms", oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["webrtc"])
        for text, value in (("0", 0), ("40", 40)):
            with mock.patch.dict(os.environ, {oxrtc.SOUND_LAG_ENV: text}):
                self.assertEqual(oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["webrtc"]["sound_lag_ms"], value)


class LogLevel(unittest.TestCase):
    def test_the_log_level_is_info_unless_the_run_asks_for_another(self):
        import os
        from unittest import mock

        cams = [Camera(1, "rtsp://cam/main", "rtsp://cam/sub")]
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(oxrtc.LOG_LEVEL_ENV, None)
            self.assertEqual(oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["log"], {"level": "info"})
        with mock.patch.dict(os.environ, {oxrtc.LOG_LEVEL_ENV: "debug"}):
            self.assertEqual(oxrtc.Oxrtc(Path("unused"), cams).config({"copy"})["log"], {"level": "debug"})



class Recovery(unittest.TestCase):
    """R01, R02: the camera's return to its first NEW picture, not a frozen one shown again."""

    DOWN, UP = 10_000.0, 20_000.0

    def frames(self, *spans):
        out = []
        for start_ms, first_code, count in spans:
            out += [[start_ms + 40 * i, first_code + i, 0] for i in range(count)]
        return out

    def test_the_frozen_keyframe_shown_again_is_not_a_recovery(self):
        from run import recovery_ms

        before = self.frames((7_000, 300, 75))  # codes 300..374, to 10 000 ms
        replays = [[13_000, 350, 0], [16_000, 350, 0], [21_000, 350, 0], [24_000, 350, 0]]
        back = self.frames((27_100, 0, 10))  # the new publisher restarts its file at 0
        self.assertEqual(recovery_ms(before + replays + back, self.DOWN, self.UP), 7_100)

    def test_a_misread_code_is_not_a_picture(self):
        from run import recovery_ms

        frames = self.frames((7_000, 300, 75)) + [[22_000, 3000, 0], [22_040, 77, 0]] + self.frames((23_000, 0, 5))
        self.assertEqual(recovery_ms(frames, self.DOWN, self.UP), 3_000)

    def test_without_codes_the_first_picture_after_the_return_counts(self):
        from run import recovery_ms

        frames = [[9_000, -1, 0], [21_000, -1, 0], [22_000, -1, 0]]
        self.assertEqual(recovery_ms(frames, self.DOWN, self.UP), 1_000)
        self.assertIsNone(recovery_ms([[9_000, -1, 0]], self.DOWN, self.UP))

    def test_no_new_picture_is_no_recovery(self):
        from run import recovery_ms

        frames = self.frames((7_000, 300, 75)) + [[21_000, 350, 0], [24_000, 350, 0]]
        self.assertIsNone(recovery_ms(frames, self.DOWN, self.UP))

    def test_the_recovery_scenarios_decode_every_viewer(self):
        for sid, viewers in (("R01", 1), ("R02", 10)):
            group = scenario.load(sid)["webrtc"][0]
            self.assertGreaterEqual(int(group["decode_first"]), viewers, sid)


if __name__ == "__main__":
    unittest.main()
