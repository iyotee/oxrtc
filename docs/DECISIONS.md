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
installs one for linux-arm64, a local build uses zig's, D17) and a release
must carry aws-lc's notices, which the archives do since R16 (R15).

## D4 - Our own RTSP server (2026-11-14)

No Rust RTSP server is both maintained and strict. The server's message
framing uses `rtsp-types` (MIT), the SDP uses `sdp-types` (MIT); the session
logic, the packetizers and the limits are ours. For the transcoders'
publications (RTSP RECORD) the RTP depacketizers are retina's own, so there is
one depacketizer implementation in the project (`crates/depacketize`, which
the camera ingest uses too since D26).

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
Changes if: the cameras' own sender reports reach the core (R11 left them
unread: D26), which would anchor a run on the camera's clock instead of its first arrival.

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
server), IPv6 relayed addresses (none is asked for). `turn:...?transport=tcp`
was in this list until D21. Changes if: an installation whose UDP to the
relay is blocked needs TURN over TLS (`turns:`); a rotation without a restart
(R3's runtime update) would keep the allocation code as it is and swap its
credentials.

## D14 - Relays are aliases resolved at selection, not sources of their own (2026-11-15)

A `stream:<name>?<query>` source, and a loopback RTSP URL on our own port
(openIVR's `rtsp://127.0.0.1:<port>/camera_N_src?video`, which the RTSP
factory turns into one), is an alias: no driver runs it. When a consumer
subscribes, each media it wants is resolved over a flat list of candidates,
an alias standing for the sources of the stream it names with the wish
narrowed by its query (`crates/core/src/select.rs`); the consumer then reads
the upstream source's own channel (a ring then, a queue per consumer since
D15), GOP cache and timeline.

Why: the relay was a second supervised driver (`LocalDriver`) that
subscribed to its upstream and published every frame again - a copy of each
frame into a second ring and GOP cache, a second stall watchdog, and a second
backoff stacked on the upstream's. After an outage longer than the stall
timeout the relay slept in its own backoff while its upstream was back
(334 to 801 ms in `a_relay_comes_back_with_its_upstream_without_a_backoff_of_its_own`
before, up to 6.25 s with the default 5 s ceiling), the idle grace was paid
twice, and every relay had its own timeline to keep in step with the
upstream's. openIVR's every camera goes through two relays (`camera_N` and
`camera_N_rec` read `camera_N_src`), so each frame was handled three times.

Resolved at selection, not when the stream is made: openIVR creates the
streams one by one over the API, so an alias may name a stream that does not
exist yet, and the stream under a name is replaced when a camera's URL
changes. A subscription keeps the streams its routes went through and, when
one closes, resolves those media again in its own stream: a replaced
upstream is followed (frames resume on a keyframe), a removed one ends the
subscription with `Closed` when it reads nothing else; a subscription that
still reads other media keeps them and asks for the lost ones again every
`backoff_max`, as the relay's own driver retried. A cycle, which a driver merely spun on, would
make resolution recurse: `Hub::upsert` refuses it explicitly (`SpecError::Cycle`,
under the write lock), per media as resolution follows them (two streams that
lend each other a different media are not one, and go2rtc serves them), and
resolution is bounded anyway (8 aliases in a row, 64 candidates and 256
streams entered per media, a stream reached again with the same wish entered
once: a fan-out of aliases that finds no source was otherwise exponential).

The cost: a relay's own `#timeout=N` has no effect (it was already ignored on
a loopback URL); the upstream's consumer list and consumer limit no longer
show or count relays; a viewer of a relay whose upstream is removed, and
that reads nothing else, is ended rather than kept waiting for a stream of
that name to come back; across a
replacement the new upstream's timeline starts on the wall clock, with no
history of what the reader was given, so a reader that moves can step back by
the lead the old source had over the wall clock (at most 10 s, D12; none for a
camera delivering in real time); `Stream::preload` (the core's, not the API's, which is a
subscription) keeps the sources picked when it was made. Changes if: a relay
needs to change what it carries (a filter that rewrites frames would need a
driver again).

## D15 - A queue per consumer, bounded by bytes and time for its class; the egresses keep none (2026-11-15)

Each consumer of a source has its own queue (`crates/core/src/queue.rs`),
bounded by bytes and by the age of its oldest item, with a budget per class:
`Live` (WebRTC, stills, preloads: 8 MiB, 2 s) and `Recorder` (RTSP readers:
32 MiB, 10 s). The producer enforces it when it publishes; beyond it the
queue keeps what follows its latest keyframe when that fits, else drops its
frames, and the drop is counted and logged. The egresses read their
subscription only when they can send - the RTSP connection while its write
queue has room, a WebRTC session once connected - so the core queue is the
only backlog, and its budget the only one to reason about.

Why: one `tokio::sync::broadcast` ring of 256 items per source, then a write
queue of 256 items per RTSP connection and 512 frames per connecting WebRTC
session, each dropping on its own count. A count says nothing a consumer can
rely on: 256 items is about 4 s of a camera's picture and its Opus sound,
less for a camera that sends slices, and the drop then waits for the next
keyframe, up to 6 s on the owner's cameras. A recorder (openIVR's segmenter,
reading RTSP) paused 5 s lost frames in the write queue (`a_reader_paused_5_s_loses_nothing`
failed with a gap in both tracks); a live viewer, the other way, was handed
4 s of the past before anything was dropped. Three queues with three bounds
were also three places where a frame could go missing. Keeping from the
latest queued keyframe, rather than waiting for the next one, makes a drop
cost the frames before it only.

A keyframe request from a viewer (R24) is answered from the cached group of
pictures, sent again to that session with RTP times of its own between the
last ones sent and the next frame, at most once a second: there is no
upstream to ask, and the browser drops a frame older than one it decoded
(so a new frame earlier than the replay's last time is written just after
it). A cut of a WebRTC session's own queue is answered the same way: a
session that connects later than the live budget otherwise found no
keyframe left in its queue and showed nothing until the camera's next one
(review of 2026-11-16).

The cost: publishing a frame locks each consumer's queue in turn (the
broadcast ring wrote once and woke each receiver); a consumer that does not
read holds up to its budget of frames, mostly shared with the other
consumers; an `ffmpeg:` transcode reads over loopback RTSP, so it was a
recorder, and a transcode that falls behind (a loaded CPU) could lag up to
10 s before it is cut to a keyframe (a video transcode says `queue=live`
since R23b, D18; a sound transcode stays a recorder); a replay's frames are
decoded in a burst within one frame interval. The
budgets are configuration since 2026-11-16 (R30, D16): `rtsp.queue_*` for
the recorders, `webrtc.queue_*` for the live class.

## D16 - The streams' limits are keys of `api:` (2026-11-16)

The hub's own limits - `api.max_streams`, `api.max_consumers_per_stream`,
`api.max_frame_mib`, `api.gop_cache_mib` - are oxrtc-only keys of the
`api:` section; the consumer queues' budgets are keys of the section of
the egress whose consumers they bound (`rtsp.queue_max_mib` and
`rtsp.queue_max_seconds` for the RTSP server's readers, `webrtc.queue_*`
for live consumers, the API's stills and preloads included since they are
of the same class). Sizes are whole MiB up to 4096, times whole seconds up
to 3600: a byte count from the file never overflows, and an age bound never
overflows a clock.

Why: COMPATIBILITY's rule puts an oxrtc-only key in the section of what it
configures, never in a section of its own, so that a go2rtc file runs
unchanged and go2rtc still reads one that carries them. What these
configure is the streams, and `streams:` cannot carry a key: go2rtc reads
every key there as a stream name (a `gop_cache_mib: 8` would become a
stream whose source is `8`). `api:` is the section of the component that
manages the streams - `PUT /api/streams` creates them, `GET /api/streams`
lists them - and go2rtc ignores keys it does not know there. The supervision
timings (`connect_timeout`, `stall_timeout`, the backoff) stay defaults: a
camera's stall timeout is already its source's `#timeout=N`, which openIVR
writes.

Changes if: the owner prefers a section of its own (`oxrtc:`), which go2rtc
would ignore as well; the keys would move, the names stay.

## D17 - Release assets from one script, Linux cross-built with zig (2026-11-16)

`scripts/release_local.py` makes a release's assets - per platform the
archive (`oxrtc`, on Linux `oxrtc-exec` beside it, the licences, the
README, `THIRD_PARTY_NOTICES.txt`, `sbom/`), the SBOMs beside it, then
`SHA256SUMS` - on a developer's machine, and `release.yml` runs the same
script per platform: one packaging, not one in YAML and another for local
builds. From Windows the Linux targets are cross-compiled with
cargo-zigbuild, zig from pip (`ziglang`) serving as linker and as the C
compiler aws-lc-sys needs; nothing asks for administrator rights or a
system-wide toolchain. The Linux binaries are linked against glibc 2.35
(zigbuild's `<target>.2.35`; Ubuntu 22.04 for `release.yml`), so they run on
Ubuntu 22.04, Debian 12 and Raspberry Pi OS Bookworm; since such a binary
cannot run where it is built, the script reads its ELF headers instead
(architecture, interpreter, glibc's libraries only, no symbol newer than
the floor, segments aligned to the platform's largest page: 64 KiB on arm64,
for the Raspberry Pi 5's 16 KiB pages) and refuses the release otherwise. The Windows binary links the
C runtime in (`-C target-feature=+crt-static`, for its target only), since
an unzipped archive cannot count on the Visual C++ Redistributable; its PE
imports are read and a C runtime DLL refuses the release.

Why: GitHub Actions minutes are blocked on the account, and the Raspberry Pi
5 needs a binary now; zig carries a C compiler and every glibc's link stubs
for every target in one user-installed package, where a gcc cross toolchain
on Windows does not exist without MSYS2 or a Linux VM (neither available:
no Docker daemon, no virtualization platform for WSL 2). Building on the Pi
itself was the other way (a Dockerfile stage), slower and needing a Rust
toolchain there, kept for when zig cannot build a target. The glibc floor is
the one `release.yml` already promised.

Changes if: zig's C compiler mis-builds aws-lc-sys (the Pi's first run in
phase D is the check; CI's gcc-built archives would then be the reference),
or a target outside glibc is needed (musl, 32-bit ARM).

## D18 - A reader names its class with `queue=`; a consumer is listed by its `User-Agent` (2026-11-16)

An RTSP reader says what it is in its URL: `?video&queue=live` is a reader
that shows what it reads and gets the live budget (8 MiB, 2 s), `queue=recorder`
or nothing the recorder's (32 MiB, 10 s). `ConsumerClass::from_query`
(`crates/core/src/queue.rs`) reads the key, `Reader` applies it
(`crates/rtsp-server/src/reader.rs`); a value that is neither, or two that
differ, is `400` rather than the default. The class a consumer got is listed
as `queue` in `/api/streams` (oxrtc only), because nothing else shows from
outside why a reader is, or is not, left behind. A consumer also carries the
`User-Agent` of its request, cleaned (control characters replaced, at most 256
characters), and `/api/streams` lists it as go2rtc does: openIVR names each
WHEP session by it (`openIVR-whep/<id>`), and a viewer's consumer is
`webrtc/whep`, go2rtc's own name.

Why: the audit of 2026-11-16 found openIVR's analysis decoder, an RTSP reader
of the camera's relay (`analysis_url()`), in the recorder class. A recorder
loses nothing for 10 s, which is right for a segmenter and wrong for a
detector: after a stall its detections trail the picture by up to 10 s where
go2rtc left it about 4 s behind. The reader is the only party that knows
which it is, and a key in the query is how go2rtc's own clients already say
`video` and `audio`; go2rtc ignores a key it does not know, so the same URL
works on both servers. The default stays the recorder: a reader that says
nothing is more likely to write what it reads than to show it, and the cost
of the wrong guess is a lost frame in a file against a late frame on a screen.
A class by stream name (`*_rec`) was refused: oxrtc would know openIVR's
naming, and a second client would have to follow it.

The cost: the class is chosen by whoever can reach the RTSP port, which is
loopback in openIVR's file; choosing `recorder` gets what an unnamed reader
gets, `live` a smaller budget, so the key grants nothing. `User-Agent` is
client data: bounded, single-line, never logged raw as part of a source.
Changes if: openIVR's own readers all say what they are (then the default
could become `live`).

Amended 2026-11-16 (R23b): the `ffmpeg:` driver says it. A transcode of a
stream of this server reads it through the same loopback RTSP (D5), so it was
a recorder, and the video transcode that feeds WebRTC viewers
(`camera_N: ffmpeg:camera_N_src#video=h264...`) could lag up to 10 s on a
loaded CPU (the Raspberry Pi) before being cut to a keyframe, where a live
consumer is cut after 2 s. The driver now adds `&queue=live` to the URL it
builds (`crates/ffmpeg/src/args.rs`, `shows_its_output`) when the source has
a `#video=` and no `#audio=`: its output is a picture somebody is looking at.
It keeps the recorder class for everything that makes sound (`#audio=`, alone
or beside `#video=`): openIVR records that output (`camera_N_rec`), and a
sound cut to a keyframe is a hole in a file, not a late frame. A source that
only repackages (neither fragment) stays a recorder, since nothing says what
it feeds. A URL input (`ffmpeg:rtsp://...`) is the operator's own and is left
as written, so openIVR's sound transcode, which reads
`rtsp://127.0.0.1:P/camera_N_src`, is unchanged; an operator's own
`#query=queue=...` wins over the driver's key (and is never doubled: the server
answers `400` to two that differ). The key reaches no other server (go2rtc
ignores it, and the source is a stream of this one).

The cost, and what is not known: a video transcode that stalls past 2 s while
the camera's last keyframe is older than that has nothing to resume at, and
its viewers see no picture until the camera's next keyframe, up to 6 s on the
owner's cameras (the live class's price, D18); they were seeing a picture up
to 10 s late instead. Not measured: the A/B on a loaded machine (a transcode
of the owner's 1080p substream on the Raspberry Pi, with and without the key)
is phase D's, and decides whether the live budget (`webrtc.queue_*`) suits a
transcode as it suits a viewer. Pinned by `crates/ffmpeg/src/args_tests.rs`
(the URL for video, none for sound, the operator's class kept, URLs left
alone), `crates/ffmpeg/tests/queue_class.rs` (real FFmpeg: the reader listed
`live`, then cut after it read nothing for 6 s, a failure before the change;
the sound transcode's `recorder`) and `crates/rtsp-server/tests/paused_reader.rs`
`a_video_transcodes_input_is_cut_to_a_keyframe_after_5_s` (the driver's own URL,
paused 5 s: resumes at a keyframe each time; without the key, nothing lost).

Review of R23b (2026-11-16), two things the amendment did not say. First, the
live budget is one for every RTSP reader that says `queue=live`: it is
`webrtc.queue_*` (8 MiB, 2 s), so an operator who tightens it for viewers
tightens the video transcodes and openIVR's analysis decoder with them
(COMPATIBILITY's key table says so now). Second, a cost that is not the
stalled reader's: a new reader is given the cached group of pictures first,
outside its queue (`Route::attach`, `crates/core/src/route.rs`: the prefix is
chained ahead of the queue), and the queue's clock starts at the attach. A
transcode that takes more than 2 s to consume that replay - a group of 6 s at
15 to 30 pictures a second, decoded and encoded by a Raspberry Pi in software,
which may well run below 1.5 times real time - has its live frames older than
2 s when the replay ends, so it is cut: it shows the replay, then nothing
until the camera's next keyframe (up to 6 s), where the recorder class would
have shown the replay and then a picture lagging by up to 10 s that shrinks as
the encoder catches up. This is read from the code, not measured: no
transcode of a real 6 s group was timed on a slow machine. It is the
other side of the same trade as the stalled case, and phase D's A/B on the Pi
(time to the first picture, then the gaps, with and without the key) must
include a cold start, not only a stall. If the Pi shows the cut at every
start, the fix belongs in the queue, not in the driver (the age bound counted
from the end of the replay for a reader that asked for it), and it must keep
a stalled reader cut: a consumer that never drains the replay still has to be
bounded, by the 8 MiB and the session timeout.

## D19 - Webcams are a driver of their own on hyper and rustls; RTP/JPEG carries what RFC 2435 can describe (2026-11-16)

`http://` and `https://` sources are `crates/http-client`: hyper's HTTP/1 client
(`client`, `http1`), rustls on aws-lc-rs with the operating system's roots
(`rustls-native-certs`), `http-auth` for Basic and Digest (retina's own
dependency, already in the graph), and our own `multipart/x-mixed-replace`
reader, the part of this nobody else writes the way cameras need and the one
that is fuzzed. The pictures are published as `Codec::Jpeg` frames, each a
keyframe, and the RTSP server sends them as RTP/JPEG (RFC 2435), which is how
openIVR reads a webcam (`camera_N_src?video` for the H.264 transcode, the
recorder and the analysis). Three choices inside it:

- **A picture per request is polled at an interval, not asked for again at
  once** (`#interval=`, 100 ms by default). go2rtc 1.9.15 repeats the `GET` as
  fast as the camera answers, which an ESP32 or a router's camera pays for in
  CPU, and which nothing downstream needs (ten pictures a second is more than
  a viewer or a detector uses). The cost: a camera that could give twenty a
  second gives ten, until `#interval=10` says otherwise.
- **A picture RFC 2435 cannot describe is dropped on the RTSP side, not
  repaired.** The format sends the scan and leaves the Huffman tables, the
  sampling and the size to the receiver (standard tables, two layouts, 8
  blocks in a byte), so a receiver rebuilding the headers from a picture
  with other tables draws noise. A gap with one warning is honest; noise is
  not. Encoders with their own tables (`optimize_coding`, FFmpeg's `mjpeg`
  by default) would have cost a webcam all of its pictures: the scan is coded
  again with the standard tables (`recode.rs`, 2026-11-16 review), losslessly
  and without decoding the picture: every symbol of a baseline scan is a run
  and a size followed by raw bits, only the code of the symbol changes. The
  owner's three were never opened from here, and which of them has such
  tables is not known; none can now cost a picture.
- **`https` is verified unless the source says `#insecure`.** The system's
  roots and the host the URL names. go2rtc 1.9.15 verifies nothing (it served a
  picture from a self-signed local server), so a webcam with a certificate of
  its own that works there needs `#insecure` in its URL here. The flag is per
  source, bare as go2rtc's flags are written (a URL with it works on both
  servers: go2rtc ignores a fragment it does not know), and logs a warning at
  each connection. Verifying by default is the safer half of a difference the
  switch makes visible; a private authority is `HttpClientConfig::extra_roots`
  and has no key in the file yet.

Why a driver and not `ffmpeg:<url>` for them: FFmpeg would read the webcam in
its own process (a process per webcam, FFmpeg's HTTP parsing as the first
line of defence, a transcode to a codec every consumer can read as the only
output, so `frame.jpeg` and the recorder's pass-through would be gone) where
go2rtc and openIVR treat a webcam as a source like any other.

New dependencies, all permissive and passing `cargo deny`: `hyper` (client
features; already built for the API's server side), `hyper-util` (its `tokio`
adapter), `http`, `http-body-util`, `http-auth` (retina's), `rustls`,
`tokio-rustls`, `rustls-webpki` (rustls'), `rustls-native-certs` and its
`schannel` / `openssl-probe` / `security-framework`, `want` (hyper's), `url`
and `memchr` (already in the graph). `rcgen` and `md-5` serve the tests only.
aws-lc-rs, the C crypto the DTLS certificate already brings (R15), is the TLS
provider too: no second C library.

Changes if: a webcam turns up above 2040 pixels or progressive (R42), or hyper's client stops reading the cameras' sloppier HTTP
(the tolerance options are on, `ignore_invalid_headers_in_responses` among
them; a camera that still breaks it is a reason to write the small client).

## D20 - `rtsps` cameras: a hook in a copy of retina, and the certificate is not checked unless the source says `#verify` (2026-11-16)

openIVR's TLS policy `preferred` probes every camera, and rewrites its URL to
`rtsps://` when it answers in TLS (`tls_policy.py`). Such a camera has to play.
retina has no TLS: its connection is a `TcpStream` it opens itself. Two ways
to give it one were weighed.

- **A loopback relay** (oxrtc listens on `127.0.0.1:0`, retina connects to it,
  the relay opens the TLS connection and copies): no change to retina, but
  every request line would name the loopback address (a Digest `uri`, a
  `Content-Base`, a camera that checks the host), any local process could race
  the accept and read what retina sends (credentials answered to a challenge
  it forges), and a listening socket per session.
- **A hook in a copy of retina** (taken): `SessionOptions::connector` makes the
  stream the session reads from the caller's own (`third_party/retina`, 0.4.22,
  the patch is in `third_party/README.md`, wired with `[patch.crates-io]` like
  `h264-reader`, D10). `Transport` gains one variant over a boxed stream, the
  `rtsps` scheme is accepted when a connector is set, and the fresh
  connections of retina's background TEARDOWN go through the connector too, so
  no plain socket is opened to a camera named `rtsps`. The request lines name
  the URL the operator wrote. It costs a copy to keep in step with retina
  (about 100 lines of difference); it goes when the hook, or TLS, is upstream.

The TLS itself is rustls on aws-lc-rs, the provider the webcams' `https` and the
DTLS certificate already bring (no new dependency: one workspace crate,
`crates/net`, now holds what `crates/http-client` and `crates/rtsp-client`
share, the TCP connect across the name's addresses and the `Tls` trust).

**The certificate is not checked by default, and `#verify` checks it.** This is
the opposite of `https` (D19), on purpose, because the two are not the same
case. An `https` webcam is a URL an operator typed, and verifying it costs one
`#insecure`. An `rtsps` camera is one openIVR **promoted**: its probe checks no
certificate (`ssl.CERT_NONE`), and the URL it writes carries no option, so a
default that verifies would fail every promoted camera, and nearly all of them
(IP cameras ship a certificate generated by the device) - the gap R8 exists to
close, closed in name only. go2rtc 1.9.15 checks nothing either. What the
default gives is TLS against a passive observer of the camera's network
(Basic credentials and pictures are no longer in the clear), not against an
active one who can answer in the camera's place; plain `rtsp` gives neither.
`#verify` is the choice for a camera with a certificate that chains to a root
(the operating system's, or `RtspClientConfig::extra_roots` for a private
authority) and names the host the URL names. A warning, once per source, says
the certificate is not checked and how to check it. `#insecure`, the flag the
`https` sources use, is accepted on `rtsps` and changes nothing; `#verify`
with `#insecure` is refused when the source is created, as is `#verify` on a
plain `rtsp` source.

Other choices: the default port is 322 (IANA's `rtsps`, which openIVR's probe
uses); the connection and handshake together get four fifths of the source's
stall timeout, so a handshake that never ends is reported as that (with the
reason in `sources[].error`) and not as a DESCRIBE nobody answered; the TLS
reason in an error is a sentence that never names the URL.

**What the review measured against go2rtc 1.9.15** (a TLS server of the review's
own, the reference binary as the client): it plays a camera whose self-signed
certificate has a **1024-bit RSA key**, and the first version of this driver did
not (`TLS: invalid peer certificate: BadSignature`: rustls will not verify a
signature of such a key, nor one of a certificate its parser finds fault with).
An unverified connection now verifies nothing, the handshake's signatures
included: they prove the peer holds the key of the certificate it sent, which
is worth nothing when the certificate is taken on faith (whoever answers in the
camera's place sends a certificate of its own and signs with its key). The two
agree on the rest: TLS 1.0 and 1.1 are refused, and so is a cipher with RSA key
exchange and no ECDHE (go2rtc's Go and rustls have both dropped them), so a
camera of that age is not playable on either. They **differ on one default**: for
`rtsps://host/path` with no port go2rtc dials **554**, this driver 322. openIVR
never writes that URL for a promoted camera (`to_tls_url` always writes the
port) and its own probe looks at 322 for a declared `rtsps` URL, so the two
agree there; a hand-written `rtsps://host/path` that worked on go2rtc because
the camera speaks TLS on 554 needs its `:554` written. Kept: 322 is what the
scheme is.

Changes if: a camera is met that needs a client certificate, the owner wants
`#verify` as the default (a key in the file, not a change of the default
here), or retina takes a connector upstream (the copy goes).

## D21 - TURN over TCP: one connection per allocation, TURN's own framing (2026-11-16)

openIVR's relay entry lists `turn:...?transport=tcp` beside the UDP URL, for
a viewer on a network that lets no UDP through and gives ICE-TCP no way to
the host (R39). The client reaches the server over one TCP connection and
asks for a UDP relayed address as before (RFC 8656 section 3.1: the
allocation's `REQUESTED-TRANSPORT` is UDP whatever the connection is); no
RFC 6062 (TCP *relayed* connections), which no browser uses.

- **The framing is TURN's, not RFC 4571's.** The task that opened this item
  said "RFC 4571-style", which is how ICE-TCP frames the WebRTC port
  (`tcp.rs`, a 16-bit length in front). TURN over TCP has no prefix: a STUN
  message is its 20-byte header and the length it declares, a ChannelData
  message is its 4-byte header and its length *padded to a multiple of four*
  (RFC 8656 section 12.5). A server framing it otherwise (coturn, the
  owner's relay) would lose the stream's alignment at the first unpadded
  ChannelData, so the client pads what it sends and the test server
  (`tests/common/turn_wire.rs`, written apart) reads the stream the same
  way and fails a client that does not.
- **One cut stream, bounded.** `stun::Framer` buffers what is read and
  returns a message when `stun::frame_len` says it is whole: the first byte
  must start a STUN message (`0x00-0x3F`) or a channel (`0x40-0x4F`), the
  cookie and the 4-byte alignment are checked as soon as the bytes are
  there, a channel number outside 0x4000-0x4FFF is refused, and no message
  is held beyond 16 KiB (a relayed WebRTC packet is under 1.5 kB). So the
  buffer holds one message and one read (4 KiB) at most, and a server that
  lies closes the connection instead of growing it. Fuzzed (`turn_stream`,
  with the framer's own invariant: the same messages whatever the reads, and
  every byte in a message, held, or read past).
- **A message beyond 16 KiB is read past, once the server has spoken.** The
  first version closed the connection at the first one. The relay is shared by
  every session, and a viewer whose IP holds a permission can send the relayed
  address a 20 kB datagram (UDP allows 64 kB), which the server hands on as one
  message: closing for it ended the allocation of every other session (and,
  repeated, kept the backoff at a minute). Over UDP such a datagram is simply
  dropped, so the framer now discards it as it arrives (a STUN header's cookie
  is still checked first) without holding it. Before the server has sent one
  whole message it is refused as before: an HTTP error page starts with `HT`,
  which reads as a channel number and a length of 21588, and must not be
  waited on for 21 kB.
- **A request is sent once.** The connection delivers it or breaks
  (RFC 8489 section 6.2.1); retransmitting would send duplicates down a
  stream. It is waited for 10 s (the RFC says 39.5 s: a server that has not
  answered in ten over a connection that works is not one to wait on), then
  the allocation is lost, as for a refresh that was never answered.
- **The connection is the allocation's.** Made with each allocation (5 s to
  open), so a restart or a new attempt has a new 5-tuple and never meets
  `437 Allocation Mismatch` with its predecessor's (the property D13 wanted
  from a socket of its own). A write that fails, a close by the server or
  bytes that are not TURN's end the allocation; the driver makes another
  after the core's jittered backoff, exactly as over UDP (`turn/retry.rs`).
  At shutdown the release (a Refresh with a lifetime of 0) is queued and the
  writer given 500 ms to flush it before the connection closes.
- **Bounds**: 256 messages queued to write per connection (beyond, dropped,
  as a full UDP buffer would); one connection per TCP URL, at most four TURN
  servers in all, as before. openIVR's entry costs two allocations (UDP and
  TCP) and a viewer gets two relayed candidates, which ICE sorts out.
- The credential never reaches a log: the new lines (connect, close, bad
  framing) name the server by its URL, as the UDP ones do
  (`turn_credentials_never_reach_the_log` runs over both transports).

Not done: `turns:` (TLS to the server) stays logged and unused; TCP keepalive
on the connection (a NAT that drops it silently is noticed by the refresh
that is not answered, at most 9 minutes and 10 s later).

Changes if: an installation needs `turns:` (TLS through a proxy that only
lets port 443 through), or a relay is met whose TCP framing differs from
RFC 8656's.

## D22 - ICE servers replaced at runtime: a new allocation beside the old, which keeps its sessions (2026-11-16)

openIVR's relay hands out TURN credentials that expire, and it wrote them to
`webrtc.ice_servers` and restarted the media server at each rotation: every
recording and every live view cut for a few seconds (R44). oxrtc keeps its
allocations in-process and can take a new credential without a restart.

- **A relay is one allocation made with one credential, and never changes
  it.** The alternative, a credential held in the relay and read at each
  refresh, was rejected: a TURN server ties an allocation to the user that
  made it (RFC 8656: a request by another user is `441 Wrong Credentials`), so the
  allocation of the old credential cannot be moved to the new one, only
  made again. A new credential is therefore a new allocation, on a new socket
  or connection (the 5-tuple of an allocation is its identity, D13, D21),
  with its own relayed address.
- **Make before break, because a session keeps the address it was offered.**
  A viewer was given the relayed address in the SDP answer and its ICE checks
  go there: the allocation that has it cannot be dropped while that session
  plays. So the new relay is made beside the old, the old is *retired* when
  the new holds (no longer offered, no longer given permissions) and ends
  when no session has a route through its relayed address, at least
  `WebRtcConfig::ice_retire_grace` (60 s, not a configuration key: at least the session idle timeout, a
  session offered the address needs that long to show its first route).
  Until the new holds, new sessions are still offered the old: a credential
  the server refuses changes nothing for them. The same credential sent
  again while it has not held (openIVR sends the one it has at each renewal)
  hands the old allocations over to the pending one, it does not retire them:
  a refused credential sent twice must not leave no relay offered at all.
- **The old allocation is never made again.** Once retired, a failure of its
  refresh (its credential expired: TURN REST credentials carry their expiry)
  ends it, with its sessions, instead of the backoff that would ask a server
  for an allocation with a credential known to be stale. A session on it
  would have lost everything at a restart; it loses its relayed path, and the
  browser's ICE restarts or the viewer reconnects.
- **Bounded**: at most eight retired allocations (the oldest ends with its
  sessions), a new call drops an allocation that never held, an unchanged
  server is not touched, the usual four TURN and four STUN servers.
- **The call replaces the whole list**, as the file does, and not only the
  credentials: STUN servers are added and removed in the same call (a probe
  with a stop of its own), a TURN server no longer listed is retired.
  `PUT /api/webrtc/ice` takes the JSON form of `webrtc.ice_servers`; it is
  oxrtc-only (go2rtc answers `404`, and openIVR then restarts, as it did).
- The file is not written (oxrtc never writes it): the next start reads what
  openIVR rewrote.

Not done: the retirement is by session routes, so a session whose ICE learned
no route through the old address in the grace loses it (it had been offered
an address nobody used); no event or counter tells an operator how many
allocations are retired (the log says when one is replaced and when one ends).

Changes if: a relay is met that refuses a second allocation of one client
address and port family (the new one is made on a new socket, so it should
not), or a credential lifetime shorter than the grace makes retired
allocations pile up (the bound ends the oldest).


## D23 - A WHEP answer names a session resource: `Location`, `PATCH` and `DELETE` (2026-11-16)

openIVR's live view (Q1-07) gives each viewer a session of its own and wants
its `DELETE` to free the media server's consumer, and its `PATCH` to hand a
trickled candidate over. go2rtc 1.9.15 can do neither (`PATCH /api/webrtc` is
`405`, `DELETE` takes the `?id=` of a WHIP session only and its consumer goes
when the peer connection closes, or about 15 s after a peer that never
connected). oxrtc can, and RFC 9725 says how.

- **The shape.** `POST /api/webrtc?src=N` answers as before (`201`,
  `application/sdp`, the complete answer, every header a go2rtc client reads)
  and adds `Location: /api/webrtc/<id>` and `Accept-Patch:
  application/trickle-ice-sdpfrag`. A client that ignores both is served as it
  was. The path form (not go2rtc's `?id=`) because it is what RFC 9725 describes
  and what a browser WHEP client follows.
- **`PATCH`** with `Content-Type: application/trickle-ice-sdpfrag` (`415`
  otherwise) takes the `a=candidate` lines of the body: `204`; at most 16 lines
  in one request and 64 trickled candidates per session (`400` beyond); a
  candidate str0m cannot read (an mDNS name) is left out as in an offer; an
  `a=ice-ufrag` other than the offer's is an ICE restart, `501` (RFC 9725 allows
  a server that does trickle but not restarts to answer so). `a=end-of-candidates`
  and every other line are accepted and ignored. For a full-ICE session the
  candidates are checked by str0m's agent; for an ICE-lite one they change
  nothing the checks of the peer do not already (the server learns the peer's
  route from its own binding requests, R27), but a relayed session still needs
  the TURN permission for the new address, which `PATCH` asks for.
- **`DELETE`** ends the session and answers `200` once the consumer has left the
  stream's listing and the caller's places are free. `GET /api/streams` shows no
  `webrtc/whep` consumer of it right after, and a viewer that comes back at once
  is not refused for the per-address limit it was holding.
- **The id** is 128 random bits from the operating system, in hex. It is a
  capability on top of the API's own authentication (every route is
  authenticated, loopback included), so it is never logged, not in a line and not
  in a field (the debug line of a request refused for want of credentials elides
  it: `resource::loggable`); an unknown, malformed or ended id is `404`, whatever
  it looked like.
  Nothing binds it to the address that made the offer: openIVR's backend makes the
  offer and a browser or a relay may delete.
- **Per-address limits** are those of the offer: the session holds its place
  (`webrtc.max_sessions_per_ip`, `max_sessions`) until it retires, and `PATCH`
  and `DELETE` are API requests under the API's own connection limits. A `DELETE`
  does not wait on anything but its session (2 s at most).

Not done: ICE restart (`PATCH` with new credentials, an answer fragment), the
`If-Match` precondition of RFC 9725, `OPTIONS` on the resource, and a way to list
a resource's candidates; a session ended by the server (idle timeout, a source
that closed) leaves a resource that answers `404`, which a client must read as
"already closed".

Changes if: a client is met that follows `Location` as an absolute URL against
another origin (a reverse proxy that rewrites paths: then the path must be
configurable), or one that needs ICE restarts to survive a network change.

## D24 - RTSP readers authenticate, loopback included; each transcoder reads with a credential of its own (2026-11-16)

`rtsp.username` and `rtsp.password` used to refuse the file (R28's first half:
a key that protects must not be ignored). They now protect the RTSP server.

- **What go2rtc 1.9.15 does** (measured against the pinned binary on a free
  port): a peer on loopback is not checked at all, a wrong password included;
  any other peer gets `401` with one `WWW-Authenticate: Basic realm="go2rtc"`
  on every request, `OPTIONS` too, and only `Basic ` (case sensitive) is
  accepted, a Digest answer is refused; a request with a wrong credential is
  answered and the connection closes, one with none keeps it.
- **Basic and Digest.** oxrtc challenges with Digest (MD5, `qop=auth`, a nonce
  per connection) and Basic, as two header lines (the header map of
  `rtsp-types` joins repeated names with a comma; `wire::split_challenges` puts
  the second back on a line of its own, because a player reads one scheme per
  line and a Basic-only one does not look behind Digest). FFmpeg takes Digest, a
  go2rtc client takes Basic. Digest costs `md-5` and `subtle`, both already in
  the lock (retina and the API); no crate is new. The password is in the file in
  clear either way, so Digest buys the wire, not the store.
- **Loopback authenticates too** (as the API does, D7). The reason is the
  threat model's local adversary (SSRF through the NVR's backend, another
  account on the machine); go2rtc's exemption is the thing a protected server
  should not copy. The price is that a go2rtc file with `rtsp.username` set and
  readers that send none stops working when pointed at oxrtc: the readers must
  send it, or `rtsp.local_auth: false` (oxrtc only) restores go2rtc's rule.
  openIVR writes no credential, so nothing of its own is affected.
- **A credential per run** instead of the operator's in every FFmpeg command
  line. `Publishers::register` gives each publication token, when the server
  asks for a credential, a user name (`run-` and 64 random bits, none of them
  the token's) and a password (128 random bits) that read for as long as the
  handle lives; `PublishLease::read_credential` hands them to the `ffmpeg:`
  driver, which writes them into the input URL when, and only when, the input
  is this server's relay and carries no user of its own (`Plan::relay_input`).
  Why not the operator's: FFmpeg's argv is world-readable on most systems and
  its error lines quote the URL (they are redacted in our log, not in the
  process listing). Why not one per process: a credential seen in one command
  line would read every stream until oxrtc restarts; a run's ends with the run.
  It is issued even with `local_auth: false`, since the relay may be reached
  from a non-loopback address.
- **A publication is not a read.** Requests on `_publish/<token>` are not asked
  for a credential: the token is one, 128 bits, taken from loopback only, and
  a token opens nothing else (a `DESCRIBE` of it is `403`).
- **Wrong credentials are slow and counted.** The answer to one is held back
  250 ms, the connection closes, and one warning per 10 s says how many were
  refused and from where, never what was sent. The connection limits (D9)
  bound the rest.
- **Review (2026-11-16).** Two readings of the same fact had drifted: who is
  "on loopback" (the publication check read `::ffff:127.0.0.1` as another
  host, the gate did not; both now call `conn::is_loopback`), and which FFmpeg
  input is "the relay" (the RTSP client takes `localhost` on the relay's port
  for it, the credential did not, so `ffmpeg:rtsp://localhost:8554/x` was
  refused with `401`; `crates/ffmpeg/src/relay.rs` says it once, and only
  when the relay itself is on loopback, so a credential is never written into a
  URL that reaches another listener). The 1,024-byte header, the largest legal
  pair (256 + 256 bytes) and the 250 ms pause had no test and now have one.
- **Not done.** No nonce expiry (the nonce is the connection's and the
  connection's own timeouts bound it), no `nc` replay check (a replay needs the
  same connection), no SHA-256 Digest, no per-user list (one pair). An
  `Authorization` header of more than 1,024 bytes or more than 16 Digest
  parameters is refused.

Changes if: a player is met that sends neither Basic nor Digest MD5 (offer
`algorithm=SHA-256` beside it), or openIVR wants several credentials.

## D25 - The wait for the previous session's TEARDOWN is learned per camera (2026-11-16)

A new session of a source waits, before its DESCRIBE, for the TEARDOWN of the
one before (retina's `SessionGroup`: some cameras keep sending on a dead
session, others ration their sessions). The wait was always bounded at 1 s
(`MAX_STALE_WAIT`), and a camera that never answers TEARDOWN (go2rtc 1.9.15
while it plays, the bench's camera) always ran it to the bound: phase B's one
loss on a hot path, L03's cold start (2.97 s against 2.49 s).

- **The wait is kept, and learned.** `StaleWait` (`crates/rtsp-client/src/stale.rs`)
  starts a source at the full 1 s. A wait that lapses at the full bound makes
  the source *silent*: its next waits are `SILENT_STALE_WAIT`, 100 ms, which a
  camera that answers in milliseconds still ends early. Any wait that ends
  before its bound makes the source *answering* again.
- **A refusal takes the shortcut back.** A connection that fails after a
  shortened wait sets the source back to the full wait: the camera may have
  said no for want of the old session's end, and the retry then waits in full.
- **What it does not change.** Which sessions retina tracks, its TEARDOWN
  attempts (5 s on the existing connection, then given up for a TCP session),
  and the stale sessions it discovers from interleaved data of a dead session
  (live555), which are waited for like the others, with one limit: a session
  that cannot end within the bound (those last 65 s) lapses the full wait like a
  silent camera does, so the source is taken for silent after it. A source
  never waits longer than before; a silent one costs the full bound once per
  source and process. Waiting less can cost one refused connection (the camera
  that rations its sessions and answers TEARDOWN after more than 100 ms): that
  refusal takes the full wait back (pinned by
  `a_refusal_after_the_short_wait_makes_the_retry_wait_in_full`).
- **Why not skip the wait for TCP sessions whose connection is closed.** The
  roadmap's first idea. The connection is open for the whole of retina's
  attempt (5 s) with a camera that never answers, so there is no closed
  connection to see.

Changes if: the owner's cameras (phase D) turn out to refuse a session while
the old one lingers and answer TEARDOWN after more than 100 ms (raise the short
wait), or the full wait should be shared by every source of one camera.

## D26 - One depacketize path: cameras read retina's packets through our capped depacketizer (2026-11-16)

retina's `Demuxed` builds an access unit from the camera's packets and caps
neither its bytes nor its pieces: a camera (or whoever speaks as one) that
sends fragments of one picture and never ends it grew the unit until the
stall timeout (15 s by default), however large the hub's `max_frame_bytes`,
which only judged the unit once whole. The depacketizer of the publications
(D4) already cut at the cap, on what goes in and on what comes out. Two ways to
give the cameras the same were weighed.

- **A patch in the copy of retina** (a cap in its H.264/H.265/AAC
  depacketizers): three codecs' code to carry through every retina release,
  for a cap the project already has in one place.
- **The raw packets into our depacketizer** (taken): `Session<Playing>` is a
  stream of RTP packets (`PacketItem::Rtp`, `raw()` with the header), which
  `oxrtc-depacketize` (`crates/depacketize`, the `TrackDepacketizer` moved out
  of `crates/rtsp-server`) turns into frames with retina's own depacketizers
  inside, capped at `ProducerSink::max_frame_bytes()`. One implementation for
  cameras and publications, one fuzz harness, one set of tests. The ingest
  loop is `crates/rtsp-client/src/driver.rs`.

What changes for a camera:

- an access unit over the cap, or a packet that does not depacketize, loses
  that picture and not the session (retina's error ended the session and a
  reconnection followed); the depacketizer starts over and the next unit comes
  out whole (`an_access_unit_that_never_ends_is_cut_and_the_next_picture_plays`:
  before, no picture at all within 10 s). Each refusal is counted in the
  source's `rejected` (`ProducerSink::reject()`; an oversized unit was counted
  there when the hub refused it whole, and the cut must not make it vanish from
  `/api/streams`) and said at debug level, at most once a second;
- every packet is copied once more, from retina's buffer into the depacketizer's
  ring (the publications always did): a few hundred kB/s per camera;
- the frame's time starts at the stream's first packet, not at the RTP-Info
  anchor: the core's timeline (D12) anchors a run at its first arrival either
  way;
- the codec parameters (SPS/PPS, AAC configuration) come from the depacketizer
  (the SDP's, then the stream's), not from retina's `Stream::parameters`.

What the cap counts: the payload bytes of the packets of the unit in progress
(a packet that ends the previous unit, because the camera sets no marker bit
and only the next timestamp shows it, is the next unit's first packet, not an
addition to the one it ends: a unit that fits is never lost to it), judged after
the packet is pushed, so the memory is the cap plus one packet. retina's own
bookkeeping is on top and is not counted: a 16-byte piece and a 12-byte NAL
record per NAL unit, so a hostile stream of 4-byte NAL units holds up to about
seven times the cap in records. Bounded, and only a camera that is hostile gets
there; counting records in the cap would shrink what an honest camera may send
(by a few percent) for a bound that already holds.

The cost is one more line in the copy of retina (`third_party/retina`, the
patch in `third_party/README.md`): `Stream::format_specific_params()`, the SDP's
`a=fmtp` value, which retina parsed and kept to itself, and which the
depacketizer needs to be built (AAC's `config`, H.264's `sprop-parameter-sets`).
The camera's sender reports are still unread: retina reads them for its own
timestamps, and nothing of ours anchors on them (D12's "changes if").

Changes if: retina caps its access units (the copy and the crate can go back to
its demuxer, keeping the cap's test), or the extra copy shows in a measure
(feed the depacketizer from retina's buffer without it).


## D27 - A source that is still starting is waited for to 28 s, and a transcode may take 30 s to its first frame (2026-11-16)

Found by running the release binary on a copy of openIVR's own
`config/go2rtc.yaml` against the owner's six real cameras. Five played. The
sixth, `camera_4` (an Axis webcam over HTTPS that sends one picture a second),
never did: every WHEP offer was answered `500 no source delivered a wanted
track` after 10 s, every RTSP `DESCRIBE` of `camera_4` with `503` after 8 s, and
a retry changed nothing. go2rtc 1.9.15 on the same file answered the offer
after 28.6 s and played it, and openIVR gives a WHEP offer 30 s
(`go2rtc_manager.py` `_locked_whep_post`, `timeout=30.0`), so the switch would
have left that camera blank.

The cause is FFmpeg, not the camera: the transcode (`ffmpeg:camera_4_src#video=h264`)
reads the relay of the picture over RTSP, and FFmpeg estimates the frame rate
from 20 pictures before it opens the input (`ffprobe` of the relay: 20 s, with
go2rtc's own `-fflags nobuffer` 26 s, on go2rtc's server too). Three limits cut
that wait, each reasonable alone:

1. the subscription gave up at `probe_timeout` (8 s) and WHEP at 10 s;
2. the RTSP `DESCRIBE` waited 10 s;
3. the slot's watchdog restarts a run with no frame after
   `max(connect_timeout 15 s, #timeout 20 s)`, so FFmpeg would have been killed
   in the middle of its probe, again and again, even for a viewer who waited.

What changed:

- `Limits::first_track_timeout` (28 s). At the probe deadline, a subscription
  with no track yet, and a route whose source is still starting (not given up
  because an attempt failed), moves its deadline to this one, once
  (`Subscription::extend_for_starting_sources`). A failing source is still
  given up at its first failed attempt, and a subscription whose routes are
  all given up ends at once, so a camera that refuses the connection answers at
  8 s as before (`slow_start.rs`). The price: a camera that does not answer at
  all (powered off, off the network) is answered when its connection attempt
  fails, 15 s (16 s measured on the owner's unreachable `camera_12`), where it
  was 8 s. A subscription with a track keeps its probe deadline for
  the others (a missing sound does not hold the picture back).
- `SourceDriver::start_timeout`: the first frame a driver may take, beyond
  `connect_timeout`. The FFmpeg driver gives its own `start_timeout` (30 s, the
  time it already allows its process to publish) to the watchdog, so the two
  agree.
- WHEP's answer deadline and RTSP's `DESCRIBE` wait are 29 s, under openIVR's
  30 s and over the 28 s a source may take.

Not done, and why: `-fpsprobesize 0` in the `rtsp` template brings the probe
to 0.7 s, and was measured: the output then declares 90000 frames a second
and x264's quantizer goes from an average of 16 to 32-33 on the same
camera (a visible loss of quality), so go2rtc's behaviour is kept and waited out instead. A viewer of
`camera_4` waits about 21 s for the first picture where go2rtc made it wait
28 s.

A side finding, not a defect of oxrtc: the first picture a cold viewer is
sent must be a keyframe. The test client got none for 27 pictures on a 1080p
transcode, and the cause was the client's own UDP receive buffer (Windows'
64 KiB drops the packets of a keyframe of about 150 KiB sent at once); the
harness client now has a 4 MiB buffer, as a browser's has, and
`cold_transcode.rs` pins that a cold viewer's first picture is a keyframe on a
transcode of an H.264 camera and of a webcam.

Changes if: FFmpeg's frame-rate probe is bounded without the quality loss (an
`-fpsprobesize` that keeps the rate, or a rate carried in the relay's SDP),
after which the 28 s ceiling can come down.

## D28 - The scope is the union of go2rtc and MediaMTX; what can run a program or open the server stays off until the file allows it (2026-11-17)

Owner's decision of 2026-11-17: oxrtc is no longer "what openIVR needs of
go2rtc". It is to do what go2rtc and MediaMTX both do ([PARITY.md](PARITY.md)
lists every line and the milestone that closes it) and to measure better than
both ([BENCHMARKS.md](BENCHMARKS.md), "The rule").

What does not move is the posture that made the project (D5, D6, the threat
model): every API call authenticated, loopback included; no path that runs a
command; every buffer bounded; every parser fuzzed. Parity with servers that
have `exec:` sources and command hooks therefore takes this shape:

- **`exec:`, `echo:`, `expr:` and command hooks exist, off by default.** They
  are enabled by `exec.allow_paths` in the configuration FILE, which names the
  programs that may run; the API cannot add to that list, and a stream set
  through the API cannot use these schemes unless the file says
  `exec.allow_api: true`. SEC-007 becomes "no command execution path unless the
  file names the program": the CI check keeps refusing them with an empty list.
- **Web hooks are the default form of a hook**: an HTTP call to a URL of the
  file, no program.
- **Third-party tunnels (ngrok, pinggy, WebTorrent) are refused**: they open the
  server to the Internet through a company's relay, which is the opposite of
  the default this project sells. Remote access is TURN and a reverse proxy.
- **A new listener** (RTMP, SRT, HLS, the WebSocket API) gets what the others
  have before it ships: per-address limits, timeouts, authentication, a fuzz
  target for its parser, a line in the threat model.
- **Nothing is copied** from go2rtc or MediaMTX: both are read to know what a
  feature does and what a client expects on the wire; the code is written from
  the protocol's specification. Their names stay in the documents of this
  repository (it measures itself against them); openIVR's code names neither.
- **A C dependency is accepted where no Rust implementation exists** and the
  gain is measured (an Opus encoder for W5): through a safe wrapper crate,
  permissive licence, built by the same release script. The workspace's own
  code keeps `forbid(unsafe_code)`.

## D29 - A domain is claimed only when no row of it is lost (2026-11-17)

The bench's decision rule until now was "wins the reference scenario and loses
nowhere that matters to a NVR". The owner's rule replaces it: **better than
go2rtc and MediaMTX on every row**. In the bench's own words (`bench/tables.py`:
a row is won when oxrtc's 95 % interval is clear of every other's, tied when
intervals overlap), a release may claim a domain (WebRTC, RTSP, transcodes,
recovery, HLS...) when its rows are won or tied **with oxrtc among the best**,
and none is lost or tied behind. `bench/scoreboard.py` (W6) prints the four
counts for a session; the session of 2026-11-17 reads 28 won, 98 tied, 6 tied
behind, 15 lost of 160 with 13 rows that have no verdict.

Two consequences. A row lost for an unfair reason (a contender doing less work)
is fixed in the bench first, in the open, and measured again: it is never
argued away. And a trade (latency against smoothness under an overloaded
browser, the idle grace against the idle cost) is no longer settled by
choosing a side: both rows have to hold, which is what M4 is for.


## D30 - One MediaStream per session: the ids are rewritten in the answer's text, and the sound starts at the connection (2026-11-17)

**The defect.** An answer named a stream per media line, so a browser built
one `MediaStream` per track, and the player most WHEP and go2rtc clients are
(`video.srcObject = event.streams[0]` at every track event) ended on the
sound's stream with no picture: openIVR's live view was black for a day.
go2rtc and MediaMTX name one stream (`go2rtc` / `mediamtx`, tracks
`go2rtc-video` / `video`).

**Where the ids come from.** str0m (0.25.0, the newest) gives an answer's
media line the `a=msid` of the offer's line, or random ids per line; a
browser's `recvonly` line has none. `Media::set_msid` is private, and
`SdpApi::add_media` takes ids only for an offer of str0m's own.

**Decision 1: rewrite the answer's text** (`crates/webrtc/src/msid.rs`). The
text is the wire format (RFC 8830, RFC 5576), which str0m must keep writing
whatever it holds inside. Refused: adding `a=msid` lines to the offer before
str0m reads it (it works only because str0m copies the offer's ids, which no
document promises, and it edits the peer's description); forking str0m.
The stream id is **one per session**, 128 random bits, not a constant like
the references': two cameras on one page are two streams a page can tell
apart, and no id an offer sent is handed back. The track id is
`<stream>-<position>`. The CNAME is left as str0m writes it (one per media
line, in the SDP and in its RTCP): browsers pair the picture and the sound by
the stream id, not by the CNAME, and a CNAME rewritten in the text alone
would disagree with the RTCP.

**What it costs, measured.** One stream is also what makes a browser hold the
picture for the sound (libwebrtc synchronises the audio and the video of one
stream). L01's 55 ms against MediaMTX's 106 ms and go2rtc's 435 ms had been
measured with that synchronisation off for oxrtc alone. With the rewrite and
nothing else, L01 read 116 ms (one run): the picture waited for a sound that
sat about 90 ms deep in the browser.

**Decision 2: the sound that waited for the connection is not sent**
(`session.rs`). The depth came from the 160 ms of sound queued while ICE and
DTLS came up, sent in one burst (8 frames; `stale_sound` now counts them): the
browser's audio buffer started 164 ms deep, removed 81 ms by playing faster
and stopped at the top of its tolerance, where MediaMTX, which sends nothing
from before the connection, settles at the bottom of the same band. The
picture that waited is still sent: it is the cached keyframe a first picture
needs. After it, the browser holds oxrtc's sound and MediaMTX's alike, and
L01 is a tie between the two (102 +/- 32 ms against 140 +/- 62 ms over three
repetitions each, oxrtc the lower in every one; the roadmap's W1), both far
ahead of go2rtc, whose 128 ms sound packets make the browser hold the picture
half a second.

**On a real camera** (measured by the review of W1, 2026-11-17, on the
owner's camera_1 read through his relay, openIVR's layout, headless Edge,
90 s runs in alternation; the figure is how long the browser held each
presented picture, from its last packet to its display). The synthetic
camera's sound is clean; a real one's is not: camera_1 stops sending four
times in 30 s for 0.3 to 1.3 s, picture and sound together, and catches up in
a burst. A browser rides that by holding 0.7 to 1.1 s of sound, with or
without this decision. With a stream per track (0.1.2) it showed each picture
57 ms after its last packet (56, 46, 70) and froze for up to 1.06 s; with one
stream it holds the picture for that sound: **679 ms** (716, 767, 656, 576),
and freezes for 0.3 to 0.7 s. go2rtc: 727 and 917 ms; MediaMTX: 1100 and
844 ms, same camera, same minutes. Decision 1 therefore costs a live view
about 0.6 s on a camera whose sound stalls: what the two references cost, and
ten times the 60 ms L01 shows.

**Not decided here** (the owner's call): the picture could be released from
the sound again, for latency. Three ways, none done: the `playout-delay` RTP
header extension on the picture (not measured); oxrtc sending the sound
smoother than it receives it (cameras and FFmpeg send it in pairs, which is
what makes the browser buffer 60 ms of it; against a camera that stalls a
second it changes nothing); and, measured by the same review, the sound's
RTCP sender report sent once instead of every 5 s
(`RtcConfig::set_rtcp_report_interval_audio`): Edge then never synchronises
the two tracks, the answer still names ONE stream, and camera_1's picture is
held 142 and 64 ms. That last one gives lip sync up (the sound 0.7 s behind
the picture on that camera, as with 0.1.x) and leans on how a browser decides
to synchronise, which no specification promises.
