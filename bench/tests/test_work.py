"""bench/work.py: what a contender did, beside what it cost (W6)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import work  # noqa: E402
from clients.browser import analyze_viewer, video_kbps  # noqa: E402
from clients.rtsp_reader import RtspReader  # noqa: E402


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


def viewer(label: str, decoded: int) -> dict:
    return {"label": label, "video_stats_last": {"framesDecoded": decoded}}


class Delivered(unittest.TestCase):
    def run_of(self, **extra) -> dict:
        run = {"status": "ok", "duration_s": 100.0,
               "summary": {"cpu_pct_core_mean": 50.0, "cpu_children_pct_core_mean": 40.0},
               "browser": {"viewers": [viewer("cam5-copy-1", 3000), viewer("cam5-copy-2", 2000)]},
               "readers": [{"label": "cam5-rec-1", "count": 3000}]}
        run.update(extra)
        return run

    def test_every_picture_handed_to_a_consumer_is_counted(self):
        self.assertEqual(work.frames_delivered(self.run_of()), 8000.0)
        self.assertIsNone(work.frames_delivered(self.run_of(browser={}, readers=[])))

    def test_cpu_per_delivered_frame_charges_what_was_delivered(self):
        # 50 % of a core for 100 s is 50 s of CPU, over 8000 pictures: 6.25 ms each.
        self.assertEqual(work.cpu_per_frame("cpu_pct_core_mean")(self.run_of()), (6.25, None))
        self.assertEqual(work.cpu_per_frame("cpu_children_pct_core_mean")(self.run_of()), (5.0, None))
        # The recorder read 25 pictures where the camera sent 30: the same CPU costs more per picture.
        fewer = self.run_of(readers=[{"label": "cam5-rec-1", "count": 2500}])
        self.assertGreater(work.cpu_per_frame("cpu_pct_core_mean")(fewer)[0], 6.25)
        self.assertEqual(work.cpu_per_frame("cpu_pct_core_mean")(self.run_of(status="error", error="x")),
                         (None, "x"))

    def test_a_cameras_transcode_is_charged_per_picture_its_viewer_decoded(self):
        run = self.run_of(transcode_cpu_s_by_camera={"camera_5": 25.0})
        # Two viewers of one transcode decoded 3000 and 2000: 2500 pictures, 10 ms each.
        self.assertEqual(work.camera_cpu_per_picture("camera_5")(run), (10.0, None))
        self.assertEqual(work.camera_cpu_per_picture("camera_1")(run), (None, "no figure"))


def sample(t: float, procs: int, cpu: float, children: float, rss: float) -> dict:
    return {"t": t, "ffmpeg_procs": procs, "cpu_s": cpu, "cpu_server_s": cpu - children,
            "cpu_children_s": children, "rss_mb": rss + 50, "rss_children_mb": rss}


class AllRunning(unittest.TestCase):
    def test_a_late_transcode_is_compared_over_the_time_it_ran(self):
        # One FFmpeg from 0 s, the second from 40 s: the run mean charges the
        # first 40 s at half the work; while both run, two cores' worth.
        samples = [sample(0, 1, 0, 0, 100), sample(20, 1, 20, 18, 100), sample(40, 2, 40, 36, 200),
                   sample(60, 2, 82, 76, 210), sample(80, 2, 124, 116, 190)]
        got = work.all_running({"samples": samples})
        self.assertEqual(got["ffmpeg_procs"], 2)
        self.assertEqual(got["seconds"], 40)
        self.assertAlmostEqual(got["cpu_pct_core"], 210.0)
        self.assertAlmostEqual(got["cpu_children_pct_core"], 200.0)
        self.assertAlmostEqual(got["rss_children_mb"], 200.0)
        self.assertEqual(work.all_running_value("rss_children_mb")({"status": "ok", "samples": samples}),
                         (200.0, None))

    def test_a_slim_run_carries_it_and_a_run_with_no_transcode_has_none(self):
        self.assertEqual(work.all_running({"work_all_running": {"rss_mb": 1.0}}), {"rss_mb": 1.0})
        self.assertIsNone(work.all_running({"samples": [sample(0, 0, 0, 0, 0), sample(5, 0, 1, 0, 0)]}))
        self.assertIsNone(work.all_running({}))


class WhatEachConsumerGot(unittest.TestCase):
    def test_the_viewers_bitrate_and_tracks(self):
        stats = [{"wall": 1000.0 * i, "kind": "video", "codec": "video/H264", "bytesReceived": 900_000 * i}
                 for i in range(10)]
        stats.append({"wall": 9000.0, "kind": "audio", "codec": "audio/opus", "bytesReceived": 5})
        # 900 kB a second is 7200 kbit/s.
        self.assertAlmostEqual(video_kbps([s for s in stats if s["kind"] == "video"]), 7200.0)
        figures = analyze_viewer({"label": "cam5-copy-1", "stats": stats}, None, 40.0)
        self.assertAlmostEqual(figures["video_kbps"], 7200.0)
        self.assertEqual(figures["tracks"], ["audio:audio/opus", "video:video/H264"])
        self.assertIsNone(video_kbps(stats[:1]))

    def test_the_recorder_counts_the_picture_and_names_every_track(self):
        with tempfile.TemporaryDirectory() as tmp:
            reader = RtspReader("cam1-rec-1", "rtsp://127.0.0.1:1/camera_1_rec", 10.0, Path(tmp))
            argv = reader.argv("ffprobe")
            self.assertNotIn("-select_streams", argv, "every track is read; packets cost no decoding")
            self.assertIn("-select_streams", RtspReader("f", "rtsp://x", 10.0, Path(tmp), frames=True).argv("p"))
            reader.out_path.write_text(json.dumps({"streams": [
                {"codec_type": "audio", "codec_name": "opus", "nb_read_packets": "500"},
                {"codec_type": "video", "codec_name": "hevc", "width": 2304, "height": 1296,
                 "nb_read_packets": "143"}]}), encoding="utf-8")
            stream, count = reader._read()
            self.assertEqual((stream["codec_name"], count), ("hevc", 143))
            self.assertEqual([f"{s['codec_type']}:{s['codec_name']}" for s in reader._streams()],
                             ["audio:opus", "video:hevc"])


if __name__ == "__main__":
    unittest.main()
