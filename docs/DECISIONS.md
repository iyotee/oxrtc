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
(TURN allocation was the candidate: our own client instead, D13).

Correction (2026-11-14, found by `cargo deny`): the backend is not wholly
pure Rust. `str0m-rust-crypto` enables dimpl's `rcgen` feature, which forces
`aws-lc-rs` and so `aws-lc-sys`, C code built by `cc` for the DTLS
certificate. There is still no OpenSSL, and DTLS and SRTP themselves run on
rust-crypto, but a cross build needs a C cross compiler (`release.yml`
installs one for linux-arm64) and a release must carry aws-lc's notices,
which the archives do not yet (R15, R16).

## D4 - Our own RTSP server (2026-11-14)

No Rust RTSP server is both maintained and strict. The server's message
framing uses `rtsp-types` (MIT), the SDP uses `sdp-types` (MIT); the session
logic, the packetizers and the limits are ours. For the transcoders'
publications (RTSP RECORD) the RTP depacketizers are retina's own, so there is
one depacketizer implementation in the project.

## D5 - FFmpeg is a confined subprocess, never linked (2026-11-14, confinement 2026-11-15)

Decoders are the largest attack surface in a media server. FFmpeg runs out of
process, without a shell, without our environment, killed with its parent.
Nothing is decoded in-process. Changes if: a pure-Rust, fuzzed decoder exists
for the codecs in use.

Update (2026-11-15, R13, R29): "killed with its parent" was true on Windows
only (a job object) and "sandboxed" was not true anywhere: on Linux a
`SIGKILL` of oxrtc left every FFmpeg running, and nothing limited what one
could reach. On Linux FFmpeg is now **confined**: started through
`oxrtc-exec`, a second binary of the workspace that arms
`PR_SET_PDEATHSIG(SIGKILL)` (and checks that its parent is still oxrtc), sets
no-new-privileges, lowers the address space (4 GiB, `ffmpeg.max_memory_mib`),
file size (0), open files (256) and core size (0) limits and the niceness
(+2), restricts the file system with Landlock (read beneath the system's
binaries and libraries and the few files FFmpeg needs, write `/dev/null`,
nothing else) and scopes signals and abstract sockets, then `exec()`s FFmpeg.
It is a program rather than a `pre_exec` hook because the workspace forbids
unsafe code; rustix and landlock give safe wrappers for each call (nix would
too; rustix was already in the lock). Because the death signal follows the
starting thread, one thread that never ends starts every FFmpeg.

What it is not: the network is open (FFmpeg must reach cameras and oxrtc;
Landlock's TCP rules would need every camera port), there is no seccomp
filter, no namespace and no separate user, and a kernel without Landlock
(before 5.13, or with it disabled) keeps the death signal and the limits but
not the file-system restriction - warned at startup; an older one with
Landlock leaves out what a newer ABI brought (truncation before 6.2, device
ioctls before 6.10, the signal and abstract-socket scoping before 6.12),
named in the startup line. An installation without `oxrtc-exec` beside
`oxrtc` runs FFmpeg unconfined and untied, with a startup warning. On Windows FFmpeg is
isolated (argument vector, cleared environment) and tied to oxrtc by its job
object, not confined. Changes if: a seccomp filter is worth its maintenance
across FFmpeg versions (measure what a build calls first); an installation
needs FFmpeg to reach a file (R12's file inputs) - a per-template read path,
never a wider default; Windows gets an AppContainer or a restricted token.

## D6 - No command execution, at all (2026-11-14)

There is no `exec:` scheme and no `echo:` scheme. A source is data, never a
command line. FFmpeg arguments come from named templates in the configuration
file, which only the operator writes; the API can name a template, not supply
one.

## D7 - Authentication on loopback too (2026-11-14)

go2rtc passes loopback and Unix-socket requests without authorization. Any
process on the host - or anything that can make the NVR backend issue a
request (an SSRF) - then controls the media server. oxrtc authenticates
every API call; the go2rtc-format `api.username`/`api.password` are honoured as
HTTP Basic, and a bearer token is accepted. An unauthenticated API needs an
explicit, logged opt-in. This costs openIVR one change: send the credential it
already writes into the configuration.

## D8 - MIT OR Apache-2.0 (2026-11-14)

The Rust ecosystem's default; compatible with every dependency chosen and with
openIVR's AGPL-3.0. A GPL-3.0 Rust reimplementation of go2rtc published in
October 2026 was examined and not used as a base for that reason (and because
it ships without tests); it is a contender in the benchmark.

## D9 - Per-address limits count loopback too (2026-11-14)

Each listener has a total (`max_connections`, `max_sessions`) and a
per-address bound: the API and the RTSP server close a connection beyond it as
soon as it is accepted, WHEP answers it `503` like a full server. Loopback is
**not** exempt, for D7's reason: a local process or an SSRF through the NVR
backend arrives from loopback. An IPv4 address (IPv4-mapped included) is its
own group; a global or unique-local IPv6 address counts for its `/64`, since
one host can pick any address in it. A `/64` is a site, not a host: IPv6
viewers on one LAN subnet share one bound, as IPv4 hosts behind one NAT do. A
link-local IPv6 address counts alone, since `fe80::/64` is the prefix of every
link.
WHEP counts the address of the HTTP caller, the only one known when the offer
is answered.

The defaults are half of each total - 128 API connections, 128 RTSP
connections, 32 WHEP sessions - so one address never takes every slot. The
cost: under openIVR every client is loopback (the API and RTSP listen on
127.0.0.1, and openIVR forwards every viewer's offer from its backend), so the
NVR alone can hold half of each total: 128 RTSP readers and transcoders (about
16 cameras at 8 connections each) and 32 live views. An installation that needs
more raises the oxrtc-only keys `api.max_connections_per_ip`,
`rtsp.max_connections_per_ip`, `webrtc.max_sessions_per_ip` - up to the
totals: 256 API and 256 RTSP connections, fixed, and 64 WebRTC sessions,
raised with `webrtc.max_sessions` (live views are what an NVR multiplies). Changes if: a
deployment where one trusted proxy carries every client becomes the norm (an
allow-list of addresses counted in the total only would then fit).

## D10 - A patched copy of h264-reader, until upstream reads mislabelled levels (2026-11-14)

retina parses every H.264 SPS with h264-reader 0.9.0, which refuses a picture
larger than its declared level allows. go2rtc's own H.264 template
(`-level:v 4.1`) produces such streams above 1080p, and cameras mislabel their
level too; retina then dropped every keyframe and the stream never played,
where go2rtc plays it. A copy of 0.9.0 in `third_party/h264-reader`, wired with
`[patch.crates-io]`, bounds the picture by the largest level instead
([third_party/README.md](../third_party/README.md) has the two-hunk diff and
the test that fails without it). Why a copy and not a workaround in our code:
retina's client depacketizes camera streams internally, out of our reach, so
rewriting the SPS's level before retina sees it would cover publications only.
The cost: a dependency `cargo deny` does not see as crates.io (it is a path),
reviewed by its diff instead. **Approved by the owner on 2026-11-15**, although
it bends the crates.io-only policy of `deny.toml`. Changes if: h264-reader (or retina)
releases the same relaxation upstream (R19) - then the copy is deleted.

## D11 - The `rtsp` input template without `-fflags nobuffer` (2026-11-15)

go2rtc's `rtsp` template starts with `-fflags nobuffer`. With it FFmpeg discards
the packets it read while probing its input, and the first of them is the
keyframe this server sends every new subscriber from its GOP cache: each
transcode (openIVR's H.264 one for every H.265 camera) waited for the camera's
next keyframe, 4.8 s on the owner's cameras. Measured on bench B07 (openIVR's
layout, 10 H.265 cameras, 3 runs each, alternated): first frame 6.1 s with the
flag, 1.5 s without; glass-to-glass latency 152 ms against 107 ms; CPU over the
last 20 s of each run within 3 % (the "higher" CPU without the flag over a
whole run is the transcodes working from the start instead of idling until a
keyframe). The cost is memory: the server tree's RSS rose from about 1390 to
1920 MiB on the same runs, about 50 MiB per transcode, stable over the run
(most likely each FFmpeg's peak while it works through the cached GOP, kept
by its heap; not verified). The other input templates keep the flag: not
measured. Changes if:
phase D on the Raspberry Pi shows the transcoder, which now starts on the cached
GOP up to one GOP behind live, catching up too slowly there. An operator who
wants go2rtc's behaviour writes the template in the file's `ffmpeg:` section.

## D12 - One timeline per track in the core, anchored to the wall clock at each run (2026-11-15)

Every published frame's `pts` is rewritten onto a timeline per track
(`crates/core/src/timeline.rs`) before any consumer sees it; both egresses
send it as their RTP time and keep no continuity logic of their own (the
WebRTC egress's, which closed a restart's gap to 40 ms track by track and
missed restarts shorter than a second, is deleted). Each run of a source is
anchored at the wall-clock time its first frame of a track was received,
counted from the hub's epoch, and keeps the source's own steps within the
run; the RTSP egress dates each track's first packet with a sender report
from that frame's `received`, and every later report from that one.

Why: passed through, a source's timestamps restart with each connection, and
a long-lived reader (openIVR's segmenter) takes the restart for elapsed time:
with go2rtc it measured a sound 9.4 hours ahead of its picture. Not the
machine's clock for every frame: openIVR measured and withdrew that, since a
camera that delivers in bursts (0.6 to 1 s per full frame, then a dozen frames
in 350 ms) would carry its network's timing; within a run the camera's own
steps are its only record of when each frame was filmed. Later reports follow
the first one along the timeline rather than each frame's arrival for the
same reason: from arrivals they would move by up to a second at every report,
past the segmenter's 1 s drift allowance.

The cost: an anchor is an arrival, so a run whose first frame came late (that
camera's keyframes) is placed late by as much, for the whole run; a transcode
that starts on the cached GOP is anchored at its first output, up to a GOP
behind the content it carries (D11), and a run that ends ahead of the wall
clock hands that lead to the next one, anchored after it: no time is placed
more than 10 s ahead of its frame's arrival, so the leads of quick restarts
(measured: 45 s after ten restarts of a transcode on a 6 s GOP, before the
bound) stop there, and a source's clock that runs fast is held at that lead,
its later frames then spaced by their arrivals. WebRTC's sender reports are str0m's,
from the last frame written, so they still carry each frame's arrival.
Changes if: the cameras' own sender reports reach the core (R11), which would
anchor a run on the camera's clock instead of its first arrival.

## D13 - Our own TURN client, each allocation on a socket of its own (2026-11-15)

`webrtc.ice_servers` is honoured as openIVR writes it (one entry, `stun:` and
`turn:` URLs, a user name and credential that rotate): a `stun:` server gives
the shared socket's server-reflexive candidate, a `turn:` server over UDP a
relayed one (RFC 8656: Allocate with the long-term credential after the 401
challenge, Refresh a minute before the lifetime lapses, CreatePermission per
peer IP, ChannelBind per peer address, Send/Data indications until the
channel is bound). str0m has no TURN client (D3), and no maintained Rust one
is both Sans-I/O and small: the client is ours (`crates/webrtc/src/stun/`,
`crates/webrtc/src/turn/`), Sans-I/O like str0m, strict and bounded, its
codec checked against RFC 5769's vectors and against a test server written
apart from it, fuzzed (`turn_datagrams`, `ice_url`). HMAC-SHA1 is str0m's
crypto provider's, MD5 and CRC-32 crates already in the graph: no new crate.

One allocation per TURN server, shared by every session (ICE tells sessions
apart by their peers), kept from startup so an answer never waits for it
(at most 2 s right after startup). What a peer sends through it is handed to
the sessions as a datagram that arrived on the relayed address; what a
session sends from its relayed candidate goes to the TURN server. Each
allocation has **its own UDP socket**, not the shared one: a new process gets
a new 5-tuple, so a restart - openIVR restarts the media server at each
credential rotation, and on Windows that stop is a hard kill - never meets
`437 Allocation Mismatch` with its predecessor's allocation, which would hold
for up to its 10-minute lifetime. The STUN probe does use the shared socket:
its mapped address is that socket's.

ICE-lite stays for a session whose candidates are all host ones; a
server-reflexive or relayed candidate makes it full ICE (a lite agent has host
candidates only, RFC 8445 section 2.5, and str0m refuses the others), the
browser keeping the controlling role.

Not supported, and logged once at startup: `turns:`/`stuns:` (TLS to the
server), `turn:...?transport=tcp` (TURN over TCP), IPv6 relayed addresses
(none is asked for). Changes if: an installation whose UDP to the relay is
blocked needs TURN over TCP or TLS; a rotation without a restart (R3's
runtime update) would keep the allocation code as it is and swap its
credentials.
