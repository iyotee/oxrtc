# Decisions

Short records: the decision, why, and what would change it.

## D1 - Next generation inside, go2rtc-compatible at the edge (2026-11-14)

The internals are rethought (bounded fan-out, GOP cache, declared outputs,
authentication everywhere). The **edge** speaks exactly the part of go2rtc's
API and configuration that openIVR uses ([COMPATIBILITY.md](COMPATIBILITY.md)).
Why: it lets the benchmark swap the binary under an unchanged NVR - the only
fair comparison in situ - and lets openIVR switch without a migration.
Changes if: the native API proves enough for every consumer.

## D2 - retina for camera ingest (2026-11-14)

RTSP clients fail on cameras, not on RFCs. retina (MIT/Apache-2.0) is the
client of a production NVR and carries years of camera quirks (Reolink scaling
lists, empty fragments, FU type changes, bogus clock rates). Writing our own
would relearn each of them on users' cameras. Changes if: retina stops being
maintained, or a quirk it cannot handle blocks a camera we must support.

## D3 - str0m for WebRTC (2026-11-14)

str0m is Sans-I/O: the protocol is a state machine driven by our own sockets
and clock, which makes it testable without a network and lets one UDP socket
serve every session. It does H.264, H.265 and Opus, and offers a pure-Rust
crypto backend (`rust-crypto`, DTLS by `dimpl`) - no OpenSSL to build on
Windows or ARM. webrtc-rs was the alternative: larger, async-first, its own
runtime assumptions. Changes if: str0m lacks something only webrtc-rs has
(TURN allocation is a candidate - see the roadmap).

## D4 - Our own RTSP server (2026-11-14)

No Rust RTSP server is both maintained and strict. The server's message
framing uses `rtsp-types` (MIT), the SDP uses `sdp-types` (MIT); the session
logic, the packetizers and the limits are ours. For the transcoders'
publications (RTSP RECORD) the RTP depacketizers are retina's own, so there is
one depacketizer implementation in the project.

## D5 - FFmpeg is a sandboxed subprocess, never linked (2026-11-14)

Decoders are the largest attack surface in a media server. FFmpeg runs out of
process, without a shell, without our environment, killed with its parent.
Nothing is decoded in-process. Changes if: a pure-Rust, fuzzed decoder exists
for the codecs in use.

## D6 - No command execution, at all (2026-11-14)

There is no `exec:` scheme and no `echo:` scheme. A source is data, never a
command line. FFmpeg arguments come from named templates in the configuration
file, which only the operator writes; the API can name a template, not supply
one.

## D7 - Authentication on loopback too (2026-11-14)

go2rtc passes loopback and Unix-socket requests without authorization. Any
process on the host - or anything that can make the NVR backend issue a
request (an SSRF) - then controls the media server. rust2rtc authenticates
every API call; the go2rtc-format `api.username`/`api.password` are honoured as
HTTP Basic, and a bearer token is accepted. An unauthenticated API needs an
explicit, logged opt-in. This costs openIVR one change: send the credential it
already writes into the configuration.

## D8 - MIT OR Apache-2.0 (2026-11-14)

The Rust ecosystem's default; compatible with every dependency chosen and with
openIVR's AGPL-3.0. A GPL-3.0 Rust reimplementation of go2rtc published in
October 2026 was examined and not used as a base for that reason (and because
it ships without tests); it is a contender in the benchmark.
