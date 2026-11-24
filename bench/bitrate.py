"""The bitrate every contender's H.264 transcode is held to: the transcode rows at equal bitrate.

The owner, 2026-11-24 (D29 amended): a transcode row (the FFmpeg children's CPU,
CPU per picture, their memory, the tree's where FFmpeg dominates) is no longer
lost to a server that simply sends fewer bits. Told nothing about the rate, each
server's x264 runs at its default quality (CRF 23), and the same template gave
very different bitrates from the same camera - FFmpeg's rate control reads the
pictures' dates, which each server writes its own way: in round 1 (2026-11-23)
camera_11's encode sent 400 kbit/s under go2rtc against 3993 under oxrtc (RC4),
camera_5's 326 against 1704, B03's 4613 against 6817 (MediaMTX 2285).

So the bench, and the bench alone, gives every contender's transcode of a camera
the same rate: go2rtc and oxrtc through go2rtc's own `#bitrate=` (both write it
`-b:v R -maxrate R -bufsize R` after the video codec), MediaMTX's FFmpeg the same
three arguments. oxrtc's own defaults do not change, and no rate below what oxrtc
sent at its defaults is ever imposed (the owner's rule: never win a row by
lowering oxrtc's bitrate): each value is at least oxrtc's own mean in round 1,
rounded up, so that the others are brought up to oxrtc's rate, never oxrtc down
to theirs. The bitrate each viewer received stays a row of the tables
("picture bitrate its viewer received"), and round 1's figures (each server at
its defaults) are the published history.

`OXRTC_BENCH_BITRATE=defaults` imposes nothing: each server at its own defaults,
as before 2026-11-24 (an experiment; a session that judges a row leaves it unset).
"""

from __future__ import annotations

import os

#: Set to `defaults` to impose no rate (each server's own default quality).
ENV = "OXRTC_BENCH_BITRATE"

#: The owner's real cameras (camera id -> kbit/s): oxrtc's mean over RC1, RC4 and
#: RC5 of round 1, the highest of the three, rounded up. camera_1 and camera_2 are
#: H.264 cameras whose viewers get the copy (no transcode runs); their value only
#: holds if a transcode of them ever starts.
REAL_KBPS = {1: 6400, 2: 4600, 4: 1400, 5: 1800, 11: 4000, 12: 2400}
#: A real camera the table does not name.
REAL_OTHER_KBPS = 4000

#: The synthetic cameras, by picture size and rate (cams/media.Profile): oxrtc's
#: mean in round 1, rounded up - B02 (1920x1080 at 25: 8980 kbit/s), B03 and B07
#: (2304x1296 at 100/7, B07 scaled to 1080p's pixel count: 6817 and 7262).
SYNTHETIC_KBPS = {(1920, 1080, "25"): 9000, (2304, 1296, "100/7"): 7300}
#: A synthetic profile the table does not name.
SYNTHETIC_OTHER_KBPS = 9000


def imposed() -> bool:
    """False when `OXRTC_BENCH_BITRATE=defaults`: each server at its own defaults."""
    return os.environ.get(ENV, "").strip().lower() != "defaults"


def real(ids: list[int]) -> dict[int, int] | None:
    """kbit/s for each of the real cameras `ids`, or None when nothing is imposed."""
    if not imposed():
        return None
    return {cid: REAL_KBPS.get(cid, REAL_OTHER_KBPS) for cid in ids}


def synthetic(count: int, width: int, height: int, fps: str) -> dict[int, int] | None:
    """kbit/s for each of the synthetic cameras 1..count, or None when nothing is imposed."""
    if not imposed():
        return None
    kbps = SYNTHETIC_KBPS.get((int(width), int(height), str(fps)), SYNTHETIC_OTHER_KBPS)
    return {i: kbps for i in range(1, count + 1)}


def ffmpeg_args(kbps: int | None) -> list[str]:
    """The three FFmpeg arguments go2rtc's `#bitrate=` writes, for an FFmpeg the bench writes itself."""
    if not kbps:
        return []
    rate = f"{int(kbps)}k"
    return ["-b:v", rate, "-maxrate", rate, "-bufsize", rate]


def fragment(kbps: int | None) -> str:
    """go2rtc's `#bitrate=` fragment (oxrtc reads it the same way), or "" when none."""
    return f"#bitrate={int(kbps)}k" if kbps else ""
