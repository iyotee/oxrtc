"""oxrtc - the candidate, through its go2rtc-compatible edge (docs/COMPATIBILITY.md).

Same configuration file and API as go2rtc, by design (DECISIONS D1), with one
difference the adapter must honour: the API is authenticated on loopback too
(D7), so a per-run random username/password is written into `api:` and every
request (health, WHEP, preload) carries HTTP Basic. The binary is
target/release/oxrtc(.exe), or OXRTC_BIN.

UNTESTED: apps/oxrtc did not exist when this adapter was written; it follows
the compatibility contract, not a run. Remove this line once a run passes.
"""

from __future__ import annotations

import secrets

from servers.go2rtc import Go2rtc


# Above the largest scenario's viewer count (B05: 100).
BENCH_MAX_SESSIONS = 256


class Oxrtc(Go2rtc):
    name = "oxrtc"
    binary_name = "oxrtc"
    tested = False
    provenance = "docs/COMPATIBILITY.md and docs/DECISIONS.md (D1, D6, D7); not yet run"

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
        return config

    def version(self) -> str | None:
        from common import binary_version

        return binary_version(self.binary_path(), ["-version"]) or binary_version(
            self.binary_path(), ["--version"]
        )


ADAPTER = Oxrtc
