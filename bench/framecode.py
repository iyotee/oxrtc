"""The frame code: a 12-bit frame index burnt into every picture, and its reader.

Encoding (as the live bench of 2026-11-09 did it): a black band of BITS squares
of SQUARE px in the top-left corner of the *reference* picture (the camera's
main stream); square `bit` is white when bit `bit` of the frame number is set.
A scaled copy (the substream, a browser's downscale) keeps the code readable:
the reader scales the square size by `width / reference width`.

Timing: frame `n` leaves the publisher at `E0 + n * frame_ms`. The 12-bit code
wraps every `4096 * frame_ms`; everything here works modulo that period, so a
run longer than one wrap needs no global unwrapping.
"""

from __future__ import annotations

import math
from collections.abc import Iterable, Sequence

BITS = 12
SQUARE = 36
MODULO = 1 << BITS

#: Size of the gray thumbnail the calibration reader asks FFmpeg for: one
#: CELL x CELL block per square, area-averaged.
CELL = 4


def ffmpeg_filter(bits: int = BITS, square: int = SQUARE) -> str:
    """The drawbox chain that burns the frame number `n` into the picture."""
    parts = [f"drawbox=x=0:y=0:w={bits * square}:h={square}:color=black:t=fill"]
    for bit in range(bits):
        parts.append(
            f"drawbox=x={bit * square}:y=0:w={square}:h={square}:color=white:t=fill"
            f":enable='eq(mod(floor(n/{2**bit})\\,2)\\,1)'"
        )
    return ",".join(parts)


def thumbnail_filter(reference_width: int, bits: int = BITS, square: int = SQUARE) -> str:
    """FFmpeg filter cropping the code band of any scaled copy to a fixed gray thumbnail."""
    band_w = f"iw*{bits * square}/{reference_width}"
    band_h = f"iw*{square}/{reference_width}"
    return (
        f"crop=w='{band_w}':h='{band_h}':x=0:y=0,"
        f"scale={bits * CELL}:{CELL}:flags=area,format=gray"
    )


def decode_gray(pixels: bytes | Sequence[int], width: int, height: int, bits: int = BITS) -> int:
    """Read the code from a gray image whose `bits` squares span the full width.

    Samples the centre of each square; a square is set when brighter than 128.
    Returns -1 when the buffer is not the announced size.
    """
    if width <= 0 or height <= 0 or len(pixels) < width * height:
        return -1
    cell = width / bits
    y = min(height - 1, int(0.5 * height))
    value = 0
    for bit in range(bits):
        x = min(width - 1, int((bit + 0.5) * cell))
        if pixels[y * width + x] > 128:
            value |= 1 << bit
    return value


def decode_rgba(
    pixels: bytes | Sequence[int], width: int, video_width: int, reference_width: int, bits: int = BITS
) -> int:
    """Read the code from RGBA pixels of the band, as the browser probe does.

    `width` is the row stride of `pixels` in pixels; the square size in the
    decoded picture is SQUARE * video_width / reference_width.
    """
    if video_width <= 0 or reference_width <= 0 or width <= 0:
        return -1
    square = SQUARE * video_width / reference_width
    y = int(0.5 * square)
    value = 0
    for bit in range(bits):
        x = int((bit + 0.5) * square)
        if x >= width:
            return -1
        i = (y * width + x) * 4
        if i + 2 >= len(pixels):
            return -1
        if (pixels[i] + pixels[i + 1] + pixels[i + 2]) / 3 > 128:
            value |= 1 << bit
    return value


def step(previous: int, current: int) -> int:
    """Frames between two codes, modulo the wrap (0..MODULO-1)."""
    return (current - previous) % MODULO


#: Following codes that must agree with a code that disagrees with the
#: current anchor before the reader re-anchors on it (after a stall or a skip).
RESYNC_CONFIRMATIONS = 2


def consistent(codes: Sequence[int], max_step: int = 5) -> list[bool]:
    """Which codes are plausible: within `max_step` frames after the previous plausible one.

    A code read off a picture decoded before its first keyframe, or blurred by
    the encoder, reads as garbage; it is rejected rather than trusted. The
    first valid code of a run is accepted when the next one confirms it. A
    code that jumps away from the previous plausible one (a stall longer than
    `max_step` frames, a freeze, a keyframe wait) is accepted as a new anchor
    when the next RESYNC_CONFIRMATIONS codes follow it - otherwise one stall
    would reject every frame after it for the rest of the run. Linear time.
    """
    valid = [i for i, c in enumerate(codes) if c >= 0]
    flags = [False] * len(codes)

    def chain(k: int, needed: int) -> bool:
        """valid[k] is followed, step by step, by `needed` codes within max_step."""
        if k + needed >= len(valid):
            return False
        return all(0 <= step(codes[valid[j]], codes[valid[j + 1]]) <= max_step for j in range(k, k + needed))

    last = None
    for k, i in enumerate(valid):
        code = codes[i]
        if last is not None and step(last, code) <= max_step:
            flags[i] = True
            last = code
        elif chain(k, 1 if last is None else RESYNC_CONFIRMATIONS):
            flags[i] = True
            last = code
    return flags


def period_ms(frame_ms: float) -> float:
    return MODULO * frame_ms


def wrap(delta: float, period: float) -> float:
    """`delta` brought into [-period/2, period/2)."""
    return (delta + period / 2) % period - period / 2


def calibrate(samples: Iterable[tuple[float, int]], frame_ms: float) -> float | None:
    """The emitter's clock: E0 such that frame n left at E0 + n * frame_ms (mod the period).

    `samples` are (arrival wall ms, code) read by a fast reader close to the
    publisher. Each gives an upper bound `wall - code * frame_ms` of E0 (the
    reader's own delay is >= 0); the smallest bound is kept. Works across a wrap.
    """
    period = period_ms(frame_ms)
    bounds = [wall - code * frame_ms for wall, code in samples if code >= 0]
    if not bounds:
        return None
    ref = bounds[0]
    return ref + min(wrap(b - ref, period) for b in bounds)


def latency_ms(wall_ms: float, code: int, e0: float, frame_ms: float) -> float:
    """How long after its emission frame `code` was seen at `wall_ms`."""
    return wrap(wall_ms - (e0 + code * frame_ms), period_ms(frame_ms))


def percentile(values: Sequence[float], q: float) -> float | None:
    """Linear-interpolated percentile (0..100); None for an empty list."""
    if not values:
        return None
    ordered = sorted(values)
    pos = (len(ordered) - 1) * q / 100.0
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return ordered[lo]
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (pos - lo)
