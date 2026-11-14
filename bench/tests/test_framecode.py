"""The frame-code reader, the emitter clock, and the JS twin of the decoder.

Run: python -m unittest discover -s bench/tests   (from the repository root)
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
import unittest
from pathlib import Path

BENCH = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(BENCH))

import framecode  # noqa: E402
from common import binary  # noqa: E402


def gray_band(code: int, cell: int = framecode.CELL, noise: int = 0) -> bytes:
    """A gray thumbnail of the band as the calibration reader receives it."""
    width, height = framecode.BITS * cell, cell
    row = []
    for x in range(width):
        bit = x // cell
        on = (code >> bit) & 1
        row.append(min(255, 235 + noise) if on else max(0, 16 + noise))
    return bytes(row * height)


def rgba_picture(code: int, video_width: int, reference_width: int, rows: int | None = None) -> tuple[list[int], int]:
    """RGBA pixels of the band of a picture scaled from the reference width."""
    square = framecode.SQUARE * video_width / reference_width
    width = int(framecode.BITS * square) + 2
    height = rows or int(square) + 1
    px = []
    for _y in range(height):
        for x in range(width):
            bit = int(x // square)
            on = bit < framecode.BITS and (code >> bit) & 1
            v = 230 if on else 20
            px += [v, v, v, 255]
    return px, width


class DecodeGray(unittest.TestCase):
    def test_every_code_round_trips(self):
        width, height = framecode.BITS * framecode.CELL, framecode.CELL
        for code in range(framecode.MODULO):
            self.assertEqual(framecode.decode_gray(gray_band(code), width, height), code)

    def test_compression_noise_is_tolerated(self):
        width, height = framecode.BITS * framecode.CELL, framecode.CELL
        for noise in (-15, 15):
            self.assertEqual(framecode.decode_gray(gray_band(0b101101001011, noise=noise), width, height),
                             0b101101001011)

    def test_short_buffer_is_rejected_not_misread(self):
        self.assertEqual(framecode.decode_gray(b"\x00" * 10, 48, 4), -1)
        self.assertEqual(framecode.decode_gray(b"", 0, 0), -1)


class DecodeRgba(unittest.TestCase):
    def test_main_and_substream_scales(self):
        for video_width in (1920, 1280, 512):
            for code in (0, 1, 2048, 4095, 0b011011011011):
                px, width = rgba_picture(code, video_width, 1920)
                self.assertEqual(framecode.decode_rgba(px, width, video_width, 1920), code, video_width)

    def test_too_narrow_band_is_unreadable(self):
        px, width = rgba_picture(5, 1920, 1920)
        self.assertEqual(framecode.decode_rgba(px[: 4 * 10], 10, 1920, 1920), -1)
        self.assertEqual(framecode.decode_rgba(px, width, 0, 1920), -1)


class Consistency(unittest.TestCase):
    def test_garbage_before_the_first_keyframe_is_rejected(self):
        codes = [3001, 17, 18, 19, 20, 22]
        self.assertEqual(framecode.consistent(codes), [False, True, True, True, True, True])

    def test_wrap_is_a_small_step(self):
        self.assertEqual(framecode.consistent([4094, 4095, 0, 1]), [True] * 4)
        self.assertEqual(framecode.step(4095, 0), 1)

    def test_a_jump_is_rejected_and_the_run_continues(self):
        self.assertEqual(framecode.consistent([10, 11, 900, 12, 13]), [True, True, False, True, True])

    def test_repeated_frame_is_plausible(self):
        self.assertEqual(framecode.consistent([10, 10, 11]), [True, True, True])

    def test_a_stall_longer_than_max_step_resyncs(self):
        # 400 ms freeze at 25 fps: the browser jumps from 11 to 21; every frame
        # after it is real and must be kept, not rejected for the rest of the run.
        codes = [10, 11, 21, 22, 23, 24]
        self.assertEqual(framecode.consistent(codes), [True] * 6)
        self.assertEqual(framecode.step(11, 21) - 1, 9)  # what analyze_viewer reports as skipped

    def test_an_isolated_jump_does_not_resync(self):
        # one garbage code, then the true run continues: the garbage is not an anchor
        self.assertEqual(framecode.consistent([10, 11, 900, 901, 12, 13]),
                         [True, True, False, False, True, True])

    def test_long_garbage_is_linear_time(self):
        codes = [(i * 797) % framecode.MODULO for i in range(200_000)]  # never two in a row
        began = time.monotonic()
        flags = framecode.consistent(codes)
        self.assertLess(time.monotonic() - began, 5.0)
        self.assertEqual(sum(flags), 0)


class Clock(unittest.TestCase):
    FRAME = 40.0

    def test_calibrate_takes_the_fastest_sample(self):
        e0 = 1_000_000.0
        delays = [9, 4, 12, 4.5, 30]
        samples = [(e0 + n * self.FRAME + d, n) for n, d in enumerate(delays, start=100)]
        self.assertAlmostEqual(framecode.calibrate(samples, self.FRAME), e0 + 4)

    def test_calibrate_across_the_wrap(self):
        e0 = 5_000_000.0
        samples = []
        for n in range(4090, 4102):
            samples.append((e0 + n * self.FRAME + 7, n % framecode.MODULO))
        got = framecode.calibrate(samples, self.FRAME)
        period = framecode.period_ms(self.FRAME)
        self.assertAlmostEqual(framecode.wrap(got - (e0 + 7), period), 0.0)

    def test_latency_is_measured_modulo_the_period(self):
        e0 = 2_000_000.0
        period = framecode.period_ms(self.FRAME)
        # frame 10 of the third loop, seen 85 ms after it left
        wall = e0 + (2 * framecode.MODULO + 10) * self.FRAME + 85
        self.assertAlmostEqual(framecode.latency_ms(wall, 10, e0, self.FRAME), 85.0)
        self.assertAlmostEqual(framecode.wrap(period * 0.75, period), -period * 0.25)

    def test_no_samples_no_clock(self):
        self.assertIsNone(framecode.calibrate([], self.FRAME))
        self.assertIsNone(framecode.calibrate([(1.0, -1)], self.FRAME))

    def test_percentile_interpolates(self):
        self.assertEqual(framecode.percentile([1, 2, 3, 4], 50), 2.5)
        self.assertEqual(framecode.percentile([7], 95), 7)
        self.assertIsNone(framecode.percentile([], 95))


class JavaScriptTwin(unittest.TestCase):
    """clients/framecode.js must read what framecode.decode_rgba reads."""

    def test_js_decoder_agrees_with_python(self):
        node = binary("node")
        if node is None:
            self.skipTest("node not found (OXRTC_NODE)")
        cases = []
        for video_width in (1920, 512):
            for code in (0, 1, 1234, 4095):
                px, width = rgba_picture(code, video_width, 1920)
                cases.append({"px": px, "width": width, "vw": video_width,
                              "want": framecode.decode_rgba(px, width, video_width, 1920)})
        script = (
            "const vm=require('node:vm');const fs=require('node:fs');"
            "const ctx={};vm.createContext(ctx);"
            f"vm.runInContext(fs.readFileSync({json.dumps(str(BENCH / 'clients' / 'framecode.js'))},'utf8'),ctx);"
            "const cases=JSON.parse(fs.readFileSync(0,'utf8'));"
            "process.stdout.write(JSON.stringify(cases.map(c=>ctx.benchFrameCode.decodeRgba(c.px,c.width,c.vw,1920))))"
        )
        out = subprocess.run([node, "-e", script], input=json.dumps(cases), capture_output=True,
                             text=True, timeout=60, check=True)
        self.assertEqual(json.loads(out.stdout), [c["want"] for c in cases])


class FfmpegRoundTrip(unittest.TestCase):
    """The burnt-in code, encoded by FFmpeg and read back through the thumbnail filter."""

    def test_codes_survive_h264_encoding(self):
        ffmpeg = binary("ffmpeg")
        if ffmpeg is None:
            self.skipTest("ffmpeg not found (OXRTC_FFMPEG)")
        width = 640
        graph = f"{framecode.ffmpeg_filter()},format=yuv420p"
        encode = [ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                  f"testsrc2=size={width}x360:rate=25", "-frames:v", "40", "-vf", graph,
                  "-c:v", "libx264", "-preset", "ultrafast", "-f", "matroska", "pipe:1"]
        encoded = subprocess.run(encode, capture_output=True, timeout=120, check=True).stdout
        decode = [ffmpeg, "-hide_banner", "-loglevel", "error", "-i", "pipe:0",
                  "-vf", framecode.thumbnail_filter(width), "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"]
        raw = subprocess.run(decode, input=encoded, capture_output=True, timeout=120, check=True).stdout
        size = framecode.BITS * framecode.CELL * framecode.CELL
        codes = [framecode.decode_gray(raw[i:i + size], framecode.BITS * framecode.CELL, framecode.CELL)
                 for i in range(0, len(raw), size)]
        self.assertEqual(codes, list(range(40)))

    def test_codes_survive_the_substream_downscale(self):
        """The calibration reads the substream: code burnt at 1920, read at 512 (media.py's scale)."""
        ffmpeg = binary("ffmpeg")
        if ffmpeg is None:
            self.skipTest("ffmpeg not found (OXRTC_FFMPEG)")
        graph = f"{framecode.ffmpeg_filter()},format=yuv420p,scale=512:288"
        encode = [ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                  "testsrc2=size=1920x1080:rate=25", "-frames:v", "12", "-vf", graph,
                  "-c:v", "libx264", "-preset", "ultrafast", "-b:v", "300k", "-f", "matroska", "pipe:1"]
        encoded = subprocess.run(encode, capture_output=True, timeout=120, check=True).stdout
        decode = [ffmpeg, "-hide_banner", "-loglevel", "error", "-i", "pipe:0",
                  "-vf", framecode.thumbnail_filter(1920), "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"]
        raw = subprocess.run(decode, input=encoded, capture_output=True, timeout=120, check=True).stdout
        size = framecode.BITS * framecode.CELL * framecode.CELL
        codes = [framecode.decode_gray(raw[i:i + size], framecode.BITS * framecode.CELL, framecode.CELL)
                 for i in range(0, len(raw), size)]
        self.assertEqual(codes, list(range(12)))


if __name__ == "__main__":
    unittest.main()
