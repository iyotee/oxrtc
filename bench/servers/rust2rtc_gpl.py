"""The GPL Rust reimplementation of go2rtc (published October 2026) - a reference.

UNTESTED, and written on an assumption: no documentation of this project could
be found from the development machine (a web search on 2026-11-14 returned
nothing under that name). As a reimplementation of go2rtc it is assumed to
take go2rtc's `-config` flag, YAML file and HTTP API (`/api`, `/api/webrtc`,
`/api/preload`); every one of those assumptions must be checked against its
README before its numbers are reported. The binary is looked up as
`rust2rtc(.exe)` on PATH, or OXRTC_RUST2RTC_GPL; the runner skips this
contender while it is absent.

Licence note: the bench only runs the binary as a separate process; nothing of
its code is copied into this repository.
"""

from __future__ import annotations

from servers.go2rtc import Go2rtc


class Rust2rtcGpl(Go2rtc):
    name = "rust2rtc_gpl"
    binary_name = "rust2rtc_gpl"
    tested = False
    provenance = "ASSUMED go2rtc-compatible (no documentation found); never run"

    def version(self) -> str | None:
        from common import binary_version

        return binary_version(self.binary_path(), ["-version"]) or binary_version(
            self.binary_path(), ["--version"]
        )


ADAPTER = Rust2rtcGpl
