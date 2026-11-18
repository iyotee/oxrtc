"""oxrtc - the candidate, through its go2rtc-compatible edge (docs/COMPATIBILITY.md).

Same configuration file and API as go2rtc, by design (DECISIONS D1), with one
difference the adapter must honour: the API is authenticated on loopback too
(D7), so a per-run random username/password is written into `api:` and every
request (health, WHEP, preload) carries HTTP Basic. The binary is
target/release/oxrtc(.exe), or OXRTC_BIN.

Run on every ready scenario on 2026-11-14 (smoke runs, then R01 and R02 at
full length); the runs found and fixed four oxrtc defects, see the commits.
"""

from __future__ import annotations

import os
import secrets

from servers.go2rtc import Go2rtc


# Above the largest scenario's viewer count (B05: 100).
BENCH_MAX_SESSIONS = 256

# `OXRTC_BENCH_LIP_SYNC=0` runs oxrtc with `webrtc.lip_sync: false` (the
# picture shown as it arrives, D30); unset, the default (lip sync on).
LIP_SYNC_ENV = "OXRTC_BENCH_LIP_SYNC"

# `OXRTC_BENCH_LOG_LEVEL=debug` runs oxrtc with that `log.level`, to draw a
# run's timeline from its log (W2: when a source was found silent, each retry
# and its error). A diagnosis only: an acceptance run leaves it unset (info, as
# go2rtc's adapter writes).
LOG_LEVEL_ENV = "OXRTC_BENCH_LOG_LEVEL"


class Oxrtc(Go2rtc):
    name = "oxrtc"
    binary_name = "oxrtc"
    tested = True
    provenance = "docs/COMPATIBILITY.md and docs/DECISIONS.md (D1, D6, D7); run 2026-11-14"

    def __post_init__(self) -> None:
        self.username = "bench"
        self.password = secrets.token_urlsafe(18)

    def config(self, wants: set[str]) -> dict:
        config = super().config(wants)
        config["api"]["username"] = self.username
        config["api"]["password"] = self.password
        config.pop("srtp", None)  # not part of oxrtc's subset: would only be warned about
        # Every bench viewer offers from loopback, as openIVR's backend does, and
        # B05 holds 100 at once: the defaults (64 in all, 32 per address, D9)
        # would measure the cap, not the server. go2rtc has no such limit.
        config.setdefault("webrtc", {}).update(max_sessions=BENCH_MAX_SESSIONS, max_sessions_per_ip=BENCH_MAX_SESSIONS)
        if os.environ.get(LIP_SYNC_ENV) == "0":
            config["webrtc"]["lip_sync"] = False
        if os.environ.get(LOG_LEVEL_ENV):
            config["log"] = {"level": os.environ[LOG_LEVEL_ENV]}
        return config

    def version(self) -> str | None:
        from common import binary_version

        return binary_version(self.binary_path(), ["-version"]) or binary_version(
            self.binary_path(), ["--version"]
        )


ADAPTER = Oxrtc
