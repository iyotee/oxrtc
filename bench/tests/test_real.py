"""The real-camera mode (RC1-RC4): nothing identifying is kept, the layout is openIVR's, the figures are right.

Run: python -m unittest discover -s bench/tests   (from the repository root)
No camera, no network and no openIVR checkout is needed: a synthetic config
with documentation addresses stands in for openIVR's.
"""

from __future__ import annotations

import json
import sys
import tempfile
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
        # Asked to, a camera the owner disabled or removed is left out and named.
        absent: list[int] = []
        cams = real.load_cameras([1, 14, 99, 11], CONFIG, listed, absent=absent)
        self.assertEqual([c.id for c in cams], [1, 11])
        self.assertEqual(absent, [14, 99])

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
        server = mediamtx.Mediamtx(Path("unused"), self.cameras, layout="real",
                                   live_template="-codec:v libx264 -vf \"scale='min(iw,8)':-2\"")
        paths = server.config(set())["paths"]
        self.assertEqual(paths["camera_1_src"]["source"], "rtsp://user:s3cret@192.0.2.10:554/live/ch00_0",
                         "an RTSP camera is a native source, without go2rtc's #timeout")
        self.assertTrue(paths["camera_1_src"]["sourceOnDemand"])
        self.assertIn("runOnDemand", paths["camera_5_src"], "MediaMTX cannot read an HTTP MJPEG camera itself")
        self.assertIn("-c:v copy", paths["camera_1_rec"]["runOnDemand"])
        self.assertIn("libopus", paths["camera_1_rec"]["runOnDemand"])
        # W6, equal work: a webcam's pictures are dated as they arrive (undated,
        # FFmpeg stamped them 1/25 s apart and the recorder read 25 of 30), and
        # its recorder's stream is the picture relayed with no FFmpeg, as for
        # go2rtc and oxrtc (a webcam has no sound to make).
        for cam in ("camera_5",):
            src = paths[f"{cam}_src"]["runOnDemand"]
            self.assertLess(src.index("-use_wallclock_as_timestamps 1"), src.index(" -i "), cam)
            rec = paths[f"{cam}_rec"]
            self.assertNotIn("runOnDemand", rec, cam)
            self.assertEqual(rec["source"], f"rtsp://127.0.0.1:{server.rtsp_port}/{cam}_src")
        self.assertNotIn("use_wallclock", str(paths["camera_1_src"]), "an RTSP camera's dates are its own")
        # openIVR's live template (its `ffmpeg: h264/live:`), the scale one quoted argument.
        self.assertIn("-codec:v libx264 -vf \"scale='min(iw,8)':-2\" ", paths["camera_1"]["runOnDemand"])
        self.assertIn("real", mediamtx.Mediamtx.layouts)


class Scenarios(unittest.TestCase):
    def test_the_real_scenarios_are_valid_and_name_the_same_cameras(self):
        ids = None
        for sid in ("RC1", "RC2", "RC3", "RC4", "RC5"):
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
        for sid in ("RC1", "RC2", "RC3", "RC4", "RC5"):
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


class TheRealCase(unittest.TestCase):
    """RC5: five viewers, each in a browser process of its own, on openIVR's live grid."""

    def test_five_browsers_each_show_every_camera_in_a_tile(self):
        data = scenario.load("RC5")
        (group,) = data["webrtc"]
        self.assertEqual((group["browsers"], group["tile_px"], group["want"]), (5, 600, "copy"))
        run = real_run.RealRun(data, oxrtc.Oxrtc, 0, Path(tempfile.mkdtemp()), None)

        class Server:
            def stream(self, cam, want="copy", sub=False):
                return f"camera_{cam}"

            def whep_url(self, stream):
                return f"http://127.0.0.1:1/api/webrtc?src={stream}"

            def auth_header(self):
                return {}

        live = [1, 2, 5, 11]
        browsers = run._browsers(Server(), live)
        self.assertEqual(len(browsers), 5, "no shared browser: every viewer is one of the five people's")
        for k, (viewers, tile) in enumerate(browsers):
            self.assertEqual(tile, 600)
            self.assertEqual([v.url.rsplit("=", 1)[1] for v in viewers], [f"camera_{c}" for c in live])
            self.assertEqual({v.label for v in viewers}, {f"cam{c}-copy-b{k + 1}-1" for c in live})
        # The recorder and its hold are there, as in RC1.
        self.assertEqual(data["rtsp"], scenario.load("RC1")["rtsp"])
        self.assertEqual(data["preloads"], scenario.load("RC1")["preloads"])

    def test_the_browsers_key_is_bounded_and_the_real_layouts(self):
        base = {"id": "Z", "title": "t", "status": "ready", "measures": [], "duration_s": 10, "layout": "real",
                "cameras": {"count": 1, "ids": [1]}}
        scenario.validate({**base, "webrtc": [{"browsers": 5, "tile_px": 600}]})
        for bad in ({"browsers": 9}, {"browsers": -1}, {"browsers": "5"}, {"tile_px": 4000}):
            with self.assertRaises(scenario.ScenarioError, msg=bad):
                scenario.validate({**base, "webrtc": [bad]})
        plain = {**base, "layout": "plain", "cameras": {"count": 1}, "webrtc": [{"browsers": 2}]}
        with self.assertRaises(scenario.ScenarioError):
            scenario.validate(plain)

    def test_the_outputs_of_the_browsers_are_one_probes(self):
        merged = real_run.merge_probes([{"ua": "Edge", "viewers": [{"label": "a"}], "error": None},
                                        {"ua": "Edge", "viewers": [{"label": "b"}], "error": "crashed"}])
        self.assertEqual([v["label"] for v in merged["viewers"]], ["a", "b"])
        self.assertEqual(merged["error"], "browser 2: crashed")
        self.assertEqual(merged["ua"], "Edge")


class NoCameraAnswers(unittest.TestCase):
    """2026-11-19: the machine's network link dropped twice during a session; two runs
    found none of the six cameras, started the server, let it idle for the whole run
    and were tabulated as repetitions (a tree that costs nothing, no first frame)."""

    def test_a_run_where_no_camera_answers_is_an_error_and_starts_no_server(self):
        import tempfile
        from unittest import mock

        started = []

        class Server:
            name = "oxrtc"

            def __init__(self, *args, **kwargs):
                started.append(self)
                self.child = self.password = None
                self.pid = 0

            def start(self, wants):
                raise AssertionError("the server was started with no camera to read")

            def stop(self):
                pass

        data = scenario.load("RC3")
        with tempfile.TemporaryDirectory() as tmp,                 mock.patch.object(real, "load_config", return_value=CONFIG),                 mock.patch.object(real, "load_cameras", return_value=real.load_cameras([1, 5, 4], CONFIG)),                 mock.patch.object(real.RealCamera, "reachable", return_value=False),                 mock.patch.object(real_run, "background_load", return_value={}),                 mock.patch.object(real_run, "binary_version", return_value="test"),                 mock.patch.object(real_run, "SETTLE_S", 0):
            record = real_run.RealRun(data, Server, 0, Path(tmp), 0.01).execute()
        self.assertEqual(record["status"], "error")
        self.assertIn("no camera", record["error"])
        self.assertEqual(started, [])
        self.assertEqual({c["reachable"] for c in record["cameras"].values()}, {False})


class OneCameraSetPerSession(unittest.TestCase):
    """A session compares the contenders on the cameras its first run measured.

    On 2026-11-21 the owner disabled camera_5 and camera_11 and enabled camera_12
    in the middle of an RC5 session: its first runs measured cameras 1, 2, 5 and
    11, a later one 1, 2, 5, 11 and 12, the last ones 1, 2 and 12, and the
    tables averaged the three sets into one row per contender."""

    class Server:
        name = "oxrtc"
        started: list = []

        def __init__(self, *args, **kwargs):
            self.child = self.password = None
            self.pid = 0

        def start(self, wants):
            self.started.append(wants)
            raise AssertionError("started")

        def stop(self):
            pass

    def _run(self, pinned, listed, answers=False):
        from unittest import mock

        config = {**CONFIG, "streams": {k: v for k, v in CONFIG["streams"].items() if k != "camera_5_src"}}
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(real, "load_config", return_value=config), \
                mock.patch.object(real, "load_camera_list", return_value=listed), \
                mock.patch.object(real.RealCamera, "reachable", return_value=answers), \
                mock.patch.object(real_run, "background_load", return_value={}), \
                mock.patch.object(real_run, "binary_version", return_value="test"), \
                mock.patch.object(real_run, "SETTLE_S", 0):
            run = real_run.RealRun(scenario.load("RC5"), self.Server, 1, Path(tmp), 0.01, pinned=pinned)
            return run, run.execute()

    def test_a_camera_disabled_during_the_session_ends_it(self):
        self.Server.started = []
        _, record = self._run([1, 5], [{"id": 5, "enabled": False, "url": "http://198.51.100.7/x"}])
        self.assertEqual(record["status"], "error")
        self.assertTrue(record["camera_set_changed"])
        self.assertIn("camera_5", record["error"])
        self.assertEqual(self.Server.started, [])

    def test_a_camera_enabled_during_the_session_is_left_out(self):
        run, record = self._run([1], [{"id": 5, "enabled": True, "url": "http://198.51.100.7/x"}], answers=True)
        self.assertEqual(run.ids, [1])
        self.assertEqual(list(record["cameras"]), ["camera_1"])
        self.assertNotIn("camera_set_changed", record)

    def test_the_first_run_measures_the_scenarios_cameras(self):
        run, record = self._run(None, [])
        self.assertEqual(run.ids, scenario.real_ids(scenario.load("RC5")))
        self.assertNotIn("camera_set_changed", record)

    def test_measured_ids_are_the_cameras_that_answered(self):
        # W6 review: camera_12 silent in a session's first run and streaming in
        # a later one made every row over all cameras compare two sets.
        record = {"status": "ok", "cameras": {"camera_1": {"reachable": True}, "camera_12": {"reachable": False}}}
        self.assertEqual(real_run.measured_ids(record), [1])
        self.assertIsNone(real_run.measured_ids({**record, "status": "error"}))
        self.assertIsNone(real_run.measured_ids({"status": "ok", "cameras": {"camera_1": {"reachable": False}}}))

    def test_a_pinned_camera_that_does_not_answer_makes_the_run_an_error_not_the_session(self):
        self.Server.started = []
        _, record = self._run([1], [], answers=False)
        self.assertEqual(record["status"], "error")
        self.assertEqual(record["cameras_unanswered"], ["camera_1"])
        self.assertIn("camera_1 did not answer", record["error"])
        self.assertNotIn("camera_set_changed", record)
        self.assertEqual(self.Server.started, [], "no server started for a run that compares nothing")

    def test_a_pinned_camera_that_answers_is_measured(self):
        self.Server.started = []
        _, record = self._run([1], [], answers=True)
        self.assertNotIn("cameras_unanswered", record)
        self.assertEqual(len(self.Server.started), 1, "the server was started")

    def test_the_runner_pins_the_first_runs_cameras_and_stops_at_a_change(self):
        from unittest import mock

        import run

        both = {"camera_1": {"reachable": True}, "camera_2": {"reachable": True}}
        made, records = [], [
            {"status": "ok", "cameras": both},
            {"status": "ok", "cameras": both},
            {"status": "error", "error": "the session's cameras changed: camera_2", "camera_set_changed": True},
            {"status": "ok", "cameras": {"camera_1": {"reachable": True}}},
        ]

        class Fake:
            def __init__(self, data, server_cls, rep, session_dir, duration, pinned=None):
                self.run_id = f"r{len(made)}"
                made.append(pinned)

            def execute(self):
                return records[len(made) - 1]

        class Adapter:
            name = "oxrtc"
            tested = True
            layouts = ("real",)

            @staticmethod
            def available():
                return True

        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(real_run, "RealRun", Fake), \
                mock.patch.object(run, "adapter", return_value=Adapter), \
                mock.patch.object(run, "RESULTS", Path(tmp)), \
                mock.patch.object(run, "quiet_machine", mock.MagicMock()):
            run.main(["--scenarios", "RC5", "--servers", "oxrtc", "--reps", "4", "--session", "s"])
            skipped = json.loads((Path(tmp) / "s" / "session.json").read_text(encoding="utf-8"))["skipped"]
        self.assertEqual(made, [None, [1, 2], [1, 2]])
        self.assertEqual([s["scenario"] for s in skipped], ["RC5"])
        self.assertIn("camera_2", skipped[0]["reason"])


if __name__ == "__main__":
    unittest.main()
