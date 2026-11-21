"""What a run's contender did, beside what it cost: the work the tables compare (W6).

- `oxrtc_mode`: the oxrtc mode a run used, named on every row (the owner,
  2026-11-20: "chaque mode contre son pareil", each mode against its peer;
  D30). A slim run (bench/slim.py) keeps it, as `oxrtc_mode`, without the
  server's configuration it is read from.
"""

from __future__ import annotations

import json
import re

LIVE = "live (webrtc.lip_sync: false)"
#: oxrtc 0.1.x named a stream per track, which left the browser's lip sync off
#: whatever the configuration said (D30): its runs are told apart.
STREAM_PER_TRACK = "0.1.x: a stream per track, no lip sync"
LIP_SYNC = "lip sync on (default)"
NOT_RECORDED = "not recorded"


def oxrtc_mode(run: dict) -> str:
    """The oxrtc mode of a run, from its recorded configuration."""
    if run.get("oxrtc_mode"):
        return str(run["oxrtc_mode"])
    if re.search(r"(?<![\d.])0\.1\.\d+", str((run.get("server") or {}).get("version") or "")):
        return STREAM_PER_TRACK
    text = run.get("server_config")
    if not text:
        return NOT_RECORDED
    if not isinstance(text, str):
        text = json.dumps(text)
    mode = LIVE if re.search(r"\blip_sync\W+false", text) else LIP_SYNC
    lag = re.search(r"\bsound_lag_ms\W+(\d+)", text)
    return f"{mode}, sound_lag_ms {lag.group(1)}" if lag else mode
