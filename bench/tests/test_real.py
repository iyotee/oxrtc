"""The real-camera mode (RC1-RC4): nothing identifying is kept, the layout is openIVR's, the figures are right.

Run: python -m unittest discover -s bench/tests   (from the repository root)
No camera, no network and no openIVR checkout is needed: a synthetic config
with documentation addresses stands in for openIVR's.
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import real  # noqa: E402
import real_run  # noqa: E402
import scenario  # noqa: E402
from servers import go2rtc, mediamtx, oxrtc  # noqa: E402
from servers.base import Camera  # noqa: E402

CONFIG = {
    "ffmpeg": {"bin": "ffmpeg", "h264": "-codec:v libx264 -g:v 30"},
    "streams": {
        "camera_1_src": ["rtsp://user:s3cret@192.0.2.10:554/live/ch00_0#timeout=15"],
        "camera_5_src": ["http://198.51.100.7:8082/mjpg/video.mjpg"],
        "camera_4_src": ["https://webcam.example.net/view/"],
        "camera_1": ["something"],
    },
}


class Cameras(unittest.TestCase):
    def setUp(self):
        self.cams = real.load_cameras([1, 5, 4], CONFIG)

    def test_the_line_is_kept_verbatim_for_the_server(self):
        self.assertEqual(self.cams[0].line, CONFIG["streams"]["camera_1_src"][0])
        self.assertEqual(self.cams[0].url, "rtsp://user:s3cret@192.0.2.10:554/live/ch00_0")
        self.assertEqual((self.cams[1].host, self.cams[1].port), ("198.51.100.7", 8082))
        self.assertEqual(self.cams[2].port, 443)

    def test_a_missing_camera_is_an_error(self):
        with self.assertRaises(real.RealCameraError):
            real.load_cameras([99], CONFIG, [])

    def test_a_camera_the_generated_config_lacks_is_read_from_the_camera_list(self):
        listed = [
            {"id": 11, "enabled": True, "url": "http://198.51.100.8:8082/mjpg/video.mjpg"},
            {"id": 12, "enabled": True, "url": "rtsp://192.0.2.12:8554/stream=0"},
            {"id": 14, "enabled": False, "url": "http://198.51.100.9/mjpg/video.mjpg"},
        ]
        cams = real.load_cameras([1, 11, 12], CONFIG, listed)
        self.assertEqual(cams[0].line, CONFIG["streams"]["camera_1_src"][0], "the generated config wins")
        self.assertEqual(cams[1].line, "http://198.51.100.8:8082/mjpg/video.mjpg")
        self.assertEqual(cams[2].line, "rtsp://192.0.2.12:8554/stream=0#timeout=15")
        with self.assertRaises(real.RealCameraError):
            real.load_cameras([14], CONFIG, listed)

    def test_scrub_removes_addresses_and_credentials(self):
        text = "error: rtsp://user:s3cret@192.0.2.10:554/live/ch00_0 refused; also 198.51.100.7 and webcam.example.net"
        clean = real.scrub(text, self.cams)
        self.assertEqual(real.leaks(clean, self.cams), [])
        for secret in ("s3cret", "192.0.2.10", "198.51.100.7", "webcam.example.net", "user:"):
            self.assertNotIn(secret, clean)
        self.assertIn("<camera_1>", clean)

    def test_scrub_reaches_keys_and_nested_values(self):
        out = real.scrub_obj({"a": ["x 192.0.2.10 y", {"198.51.100.7": 1}]}, self.cams)
        self.assertEqual(real.leaks(json.dumps(out), self.cams), [])

    def test_an_unknown_address_is_caught_by_the_second_check(self):
        self.assertTrue(real.has_address("redirected to 203.0.113.9"))
        self.assertFalse(real.has_address("listening on 127.0.0.1 and 0.0.0.0, ffmpeg 8.0.1"))

    def test_the_h264_template_is_openivrs(self):
        self.assertEqual(real.ffmpeg_h264_template(CONFIG), "-codec:v libx264 -g:v 30")
        self.assertIsNone(real.ffmpeg_h264_template({}))


class Layout(unittest.TestCase):
    def setUp(self):
        self.cams = real.load_cameras([1, 5], CONFIG)
        self.cameras = [Camera(c.id, c.line, "") for c in self.cams]

    def test_go2rtc_and_oxrtc_write_openivrs_streams_without_a_substream(self):
        for cls in (go2rtc.Go2rtc, oxrtc.Oxrtc):
            server = cls(Path("unused"), self.cameras, layout="real", h264_template="-codec:v libx264 -g:v 30")
            streams = server.streams(set())
            self.assertEqual(sorted(streams), ["camera_1", "camera_1_rec", "camera_1_src",
                                               "camera_5", "camera_5_rec", "camera_5_src"], cls.name)
            self.assertEqual(streams["camera_1_src"], [CONFIG["streams"]["camera_1_src"][0]])
            derived = go2rtc.openivr_streams(5, "", "", server.rtsp_port)
            for key in ("camera_5", "camera_5_rec"):
                self.assertEqual(streams[key], derived[key], "one implementation of the derived streams")
            self.assertEqual(server.config(set())["ffmpeg"]["h264"], "-codec:v libx264 -g:v 30")

    def test_the_plain_layouts_do_not_get_the_template(self):
        server = go2rtc.Go2rtc(Path("unused"), self.cameras, layout="plain", h264_template="x")
        self.assertNotIn("h264", server.config({"copy"}).get("ffmpeg", {}))

    def test_mediamtx_has_its_own_means(self):
        server = mediamtx.Mediamtx(Path("unused"), self.cameras, layout="real", h264_template="-codec:v libx264")
        paths = server.config(set())["paths"]
        self.assertEqual(paths["camera_1_src"]["source"], "rtsp://user:s3cret@192.0.2.10:554/live/ch00_0",
                         "an RTSP camera is a native source, without go2rtc's #timeout")
        self.assertTrue(paths["camera_1_src"]["sourceOnDemand"])
        self.assertIn("runOnDemand", paths["camera_5_src"], "MediaMTX cannot read an HTTP MJPEG camera itself")
        self.assertIn("-c:v copy", paths["camera_1_rec"]["runOnDemand"])
        self.assertIn("libopus", paths["camera_1_rec"]["runOnDemand"])
        self.assertIn("scale=1920:1080", paths["camera_1"]["runOnDemand"])
        self.assertIn("-codec:v libx264", paths["camera_1"]["runOnDemand"])
        self.assertIn("real", mediamtx.Mediamtx.layouts)


class Scenarios(unittest.TestCase):
    def test_the_four_scenarios_are_valid_and_name_the_same_cameras(self):
        ids = None
        for sid in ("RC1", "RC2", "RC3", "RC4"):
            data = scenario.load(sid)
            self.assertEqual(scenario.layout(data), "real", sid)
            self.assertEqual(data["status"], "ready")
            ids = ids or scenario.real_ids(data)
            self.assertEqual(scenario.real_ids(data), ids, sid)
            self.assertEqual(scenario.camera_list({"cameras": "all"}, data["cameras"]["count"], ids), ids)

    def test_the_cameras_of_a_real_scenario_are_not_ours_to_cut(self):
        base = {"id": "Z", "title": "t", "status": "ready", "measures": [], "duration_s": 10, "layout": "real",
                "cameras": {"count": 1, "ids": [1]}}
        scenario.validate(base)
        for bad in ({"faults": [{"at_s": 1, "cameras": "all", "down_s": 5}]}, {"cameras": {"count": 2, "ids": [1]}},
                    {"cameras": {"count": 1}}, {"cameras": {"count": 2, "ids": [1, 1]}}):
            with self.assertRaises(scenario.ScenarioError, msg=bad):
                scenario.validate({**base, **bad})
        with self.assertRaises(scenario.ScenarioError):
            scenario.validate({**base, "layout": "plain"})

    def test_the_scenario_files_hold_no_address(self):
        for sid in ("RC1", "RC2", "RC3", "RC4"):
            text = (scenario.SCENARIOS / f"{sid}.yaml").read_text(encoding="utf-8")
            self.assertFalse(real.has_address(text), sid)
            self.assertNotIn("rtsp://", text)
            self.assertEqual(yaml.safe_load(text)["layout"], "real")


class Figures(unittest.TestCase):
    def test_a_freeze_is_a_gap_above_one_second(self):
        frames = [[t, -1, i] for i, t in enumerate(list(range(0, 10000, 100)) + list(range(12500, 20000, 100)))]
        figures = real_run.freeze_figures(frames, skip_s=1.0)
        self.assertEqual(figures["freezes_1s"], 1)
        self.assertAlmostEqual(figures["frozen_s"], 2.6, places=1)
        self.assertAlmostEqual(figures["longest_gap_ms"], 2600, delta=1)

    def test_a_slow_source_is_not_frozen_at_every_picture(self):
        self.assertEqual(real_run.freeze_threshold_ms(25.0), 1000.0)
        self.assertEqual(real_run.freeze_threshold_ms(None), 1000.0)
        self.assertEqual(real_run.freeze_threshold_ms(1.0), 2500.0)
        frames = [[t, -1, i] for i, t in enumerate(range(0, 60000, 1000))]
        self.assertEqual(real_run.freeze_figures(frames, 1000.0)["freezes_1s"], 0)
        self.assertEqual(real_run.freeze_figures(frames, real_run.freeze_threshold_ms(1.0))["freezes_1s"], 0)

    def test_a_steady_stream_has_none(self):
        frames = [[t, -1, i] for i, t in enumerate(range(0, 20000, 40))]
        figures = real_run.freeze_figures(frames)
        self.assertEqual(figures["freezes_1s"], 0)
        self.assertAlmostEqual(figures["fps_steady"], 25.0, places=1)

    def test_too_few_frames_say_nothing_rather_than_zero(self):
        self.assertIsNone(real_run.freeze_figures([[0, -1, 0]])["freezes_1s"])
        self.assertIsNone(real_run.freeze_figures([])["fps_steady"])

    def test_decoded_fps_comes_from_getstats(self):
        stats = [{"kind": "video", "wall": 1000.0 * i, "framesDecoded": 25 * i} for i in range(0, 30)]
        self.assertAlmostEqual(real_run.stats_fps({"stats": stats}), 25.0, places=2)
        self.assertIsNone(real_run.stats_fps({"stats": []}))


if __name__ == "__main__":
    unittest.main()
