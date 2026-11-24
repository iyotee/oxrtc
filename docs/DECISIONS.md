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

**Amended 2026-11-24 (M4 fix round 1): the first wait is the short one, and a
silent camera is not waited for at all.** Round 1 of the full bench
(2026-11-23, six runs) still lost L03 to go2rtc (2705 against 2455 ms), and
the whole gap was one offer of twenty: each offer is delayed by the same
spread on every server, so they pair, and the first frame of nineteen of them
is the camera's next keyframe on all three servers within 15 ms; on offer 5
go2rtc showed its first frame 115 ms after the offer, oxrtc 4910 ms after
it. oxrtc's answer to that offer took 1019 ms: the full wait, again, because
the wait of offer 2 had ended at 744 ms when retina's own attempt ended, which
the rule read as an answer. That second made it miss the keyframe go2rtc
caught, and wait for the next one, 4.8 s later. So:

- a source's first wait is `SILENT_STALE_WAIT` (100 ms), long enough for a
  camera on a LAN to answer;
- a camera that has not answered within a wait is *silent* and is not waited
  for again (budget zero): its old session ends with its TCP connection, which
  retina closes after its one TEARDOWN attempt, and waiting only delays the
  first picture;
- a camera that answered within a wait waits in full from then on (it ends the
  wait in milliseconds);
- a connection that fails after a wait shorter than the full one (none, or the
  short one) takes the full wait back, as before (owner's answer of
  2026-11-19, D34: retrying a camera that refuses at once is accepted, his XM
  cameras included).

A silent camera now costs 100 ms once per source and process, where it cost
the full second. `a_camera_that_never_answers_teardown_costs_the_short_wait_once`
failed before (the first waited 1.03 s); the refusal and the answering-camera
tests are unchanged and pass.

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

**Amended by the owner, 2026-11-19: the real deployment is what is won.**
oxrtc and openIVR run on bare metal inside a home or a small site, not as a
service with customers: two to five viewers at the same time at most ("personne
ne va avoir 50-100 monitors [...] jamais y'aura plus de 2-5 clients grand max":
nobody will have 50 to 100 monitors, there will never be more than 2 to 5
clients at most). So B05 (100 viewers in one browser) and B06 (50 viewers) are
**stress rows**: still run, reported and never made worse, but outside the
count of rows to win, and no server-side trick that costs latency or lip sync
in the real case is taken to win them (D35). What must be won instead is the
real case: up to five viewers, each in a browser of its own (a separate
browser process, as five people on five devices), each showing openIVR's live
grid of the owner's cameras; W6 adds that scenario to the bench.


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

**Not decided here, 2026-11-17** (the owner's call, decided below): the picture could be released from
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

**Decided by the owner, 2026-11-17 (W1b):** the picture synchronised with
the sound, as go2rtc and MediaMTX do, "but we must still win on performance,
latency" (his words, translated). The DEFAULT is the picture held for the sound (one stream, lip
sync), as the two references do, and the latency is to be won in that mode;
the live picture is an option.

**Decision 3: the sound leaves on its stream's own clock** (`pace.rs`,
`session_sound.rs`). Each sound frame a session sends is given the instant it
leaves from the stream's clock as oxrtc can know it (the earliest arrival of
the last 2 s against the frames' own times): the pairs a camera and FFmpeg
deliver every 40 ms leave 20 ms apart, held 40 ms at most (inside ATSC
IS-191's 45 ms for a sound that lags), and each frame is dated in the sender
reports when it leaves, so the browser's lip sync sees the sound as it is
sent. Measured on L01 (the synthetic camera, 3 repetitions, abba): the browser
holds 40.3 +/- 10.1 ms of oxrtc's sound against 64.1 +/- 10.5 for MediaMTX
(won) and 66 ms before this decision.

**Decision 3 amended by the review of W1b (2026-11-18): the pacing is OFF by
default** (`Playout::pace_sound`, no configuration key; the bound of decision
4 works without it). The review measured what reaches the viewer from the
picture and the sound themselves (the camera's beep heard against its flash
seen, `bench/clients/browser.py` `av_offsets`) and found the pacing's gain on
L01 was the lip sync moved, not a shorter path: oxrtc alone, 120 s, paced
`4534be9` against unpaced `f88bda6` in alternation, the latency 70.0 and 72.5
ms paced (101.9 in a third run, the browser's buffer at 99.6) against 89.7,
91.0 and 90.4 unpaced, but the sound 16 to 22 ms further behind the picture
(A/V 84.5 and 77.9 against 61.7-63.8), so that latency plus A/V - the sound's
path from the camera to the ear - is 150-154 ms in both. The server held the
sound 38 ms on average (`sound_hold_mean_ms`) for the 24 ms the browser no
longer held. And it cost a core's share per viewer: 1.53-1.62 % against
0.56-0.60, which lost L01's CPU row (1.54 +/- 0.33 against 0.90 +/- 0.19 for
go2rtc and 0.88 +/- 0.11 for MediaMTX, the review's acceptance session
`rv-accept-L01-sync`).
Unpaced, bounded (the new default), `--reps 3 --order abba`, session
`rv-accept-L01-unpaced`: latency 90.6 +/- 25.7 ms against 135.6 +/- 67.7
(MediaMTX) and 443.7 +/- 67.1 (go2rtc), still no difference with MediaMTX;
the CPU row a tie again with oxrtc lowest (0.63 +/- 0.36 against 0.86 and
1.01); A/V by content 65.4 +/- 25.9 against 41.1 +/- 71.7 and -208.3 +/-
76.0 (go2rtc's sound is 0.2 s AHEAD of its picture: it dates its tracks
apart, whatever its answer names).
Lip sync off with the unpaced default (`OXRTC_BENCH_LIP_SYNC=0`, same
method, session `rv-accept-L01-live`): latency 8.0 +/- 0.5 ms (P95 14.8 +/-
0.6) against 110.3 +/- 53.8 and 498.6 +/- 128.1, **oxrtc better** on both
rows; the sound then 173.6 +/- 39.4 ms behind the picture by content against
MediaMTX's 83.9 (lip sync on), CPU a tie (0.64), RSS won.

**Decision 4: the sound a stall kept back is not sent beyond a bound**
(`webrtc.max_sound_late_ms`, 400 ms by default; `0` sends all of it). A frame
further behind the stream's clock than the bound is counted (`late_sound`) and
not sent - the rule `Sender::stale` applies at the connection -, unless the
source is that late in nearly every second (the lowest fifth of its last 30
seconds: a source that delivers a second at a time is not cut, a camera that
stalls in half of its seconds is). The trade, measured on the owner's
camera_1: the browser no longer buffers the stall, so the picture it holds
for the sound is about halved; the sound of the stall is lost (concealed by
the browser) instead of delayed. 400 rather than 200: the same picture hold
when the camera stalls for seconds (the browser then holds the sound for the
picture's own bursts), and at 03:00, when it only stuttered, 8.1 % of the sound
concealed against 13.5 %.

**Decision 5: lip sync off is an option** (`webrtc.lip_sync: false`). The
answer accepts the `playout-delay` header extension Chromium offers on its
video line and every picture carries 0 to 0 ("show it as soon as it is
decoded"): Edge then never holds the picture for the sound, and the answer
still names one stream. Cheap (the extension is str0m's, a few lines) and
proven, so it is kept; not the default. The sender-report experiment of the
review was not needed and is not shipped: it leans on when a browser decides
to synchronise, which nothing promises.

**The owner's table** (camera_1 through his relay, openIVR's layout, headless
Edge, 90 s runs, two rounds in alternation, 2026-11-18 11:55-12:26; the camera
had 6 gaps over 1 s a minute and 26 % of its sound more than 200 ms late in
those minutes; ms unless said):

| | picture held | sound held | sound behind picture | freezes in 90 s | sound concealed |
|---|---|---|---|---|---|
| **oxrtc, lip sync (default, 400 ms)** | **462** | 635 | -111 | 18.2 s | 34.2 % |
| oxrtc, lip sync, 200 ms | 349 | 307 | -197 | 20.2 s | 33.3 % |
| oxrtc, lip sync, no bound (W1) | 984 | 1318 | 304 | 17.7 s | 3.4 % |
| oxrtc, lip sync off (400 ms) | 11 | 591 | 482 | 24.5 s | 33.6 % |
| oxrtc, lip sync off, no bound | 13 | 1311 | 1473 | 24.7 s | 7.8 % |
| oxrtc 0.1.2 (a stream per track) | 116 | 1185 | (none) | 16.9 s | 2.3 % |
| go2rtc 1.9.15 | 896 | 1147 | 290 | 3.2 s | 2.1 % |
| MediaMTX 1.21.3 | 1540 | 1137 | -284 | 2.6 s | 2.4 % |

"Sound behind picture" is by the servers' own clocks (the two tracks'
estimated playout timestamps). The references' deep hold is also what hides a
camera's stalls: they freeze 3 s in 90 where every low-latency mode freezes 16
to 25 s. That is the price of the latency on a camera that stalls, and the
owner's to weigh.

**L01 with these decisions** (`6edcaf4`, `--reps 3 --order abba`, full
durations, 2026-11-18). Lip sync on (the default): latency mean 82.4 +/- 25.8
ms against 110.0 +/- 16.3 for MediaMTX and 505.8 +/- 111.1 for go2rtc, P95
106.2 +/- 35.3 against 129.5 +/- 26.2 and 566.8 +/- 90.0: oxrtc is the lower
in each repetition (90.0, 86.6, 70.6 against 117.1, 108.7, 104.1) and the
intervals overlap, so both rows read **no difference (oxrtc, MediaMTX)**:
the row is NOT won back in this mode. The browser holds 40 ms of oxrtc's
sound against 64 (won), yet the picture waits 5 to 32 ms longer than the
sound for oxrtc and 10 to 16 ms for MediaMTX: what remains is the browser's
own side of the sync (its audio output) and how each server dates the two
tracks (oxrtc by their arrival, after an encoder that is slower for the
picture than for the sound; MediaMTX by the source's timestamps). Lip sync
off: 8.3 +/- 0.7 ms against 117.3 +/- 25.8 and 523.8 +/- 94.2, P95 15.7 +/-
1.0: **oxrtc better** on both rows. Tried and refused: G.711 sent in 10 ms
packets (the browser held 54 ms of sound instead of 40, the picture no less,
and the server's CPU row was lost to MediaMTX, 2.86 against 1.40 % of a core).

**The owner's answers, 2026-11-19.** (1) `webrtc.max_sound_late_ms` keeps its
default of 400 ms. (2) With lip sync on, the sound MAY be dated 20 to 40 ms
later than it arrived, within the 45 ms ATSC IS-191 allows for sound behind
the picture, so that L01's latency rows are won in the default mode: that is
roadmap item W1c. (3) The live mode (`webrtc.lip_sync: false`) stays an
option, not the default; openIVR offers it as a setting (roadmap item IVR).

**Decision 6 (W1c, 2026-11-20): the sound is dated after its picture, 20 ms
by default** (`webrtc.sound_lag_ms`, 0 to 40; `session_sound.rs`
`picture_lead`, `dated`). The owner's answer (2) above, in his terms: yes to
a sound dated 20 to 40 ms later than it arrived, inside ATSC IS-191's 45 ms.

*How it is written.* A browser synchronises from the two tracks' RTCP sender
reports, so only the difference between their datings exists for it. str0m
takes no date in the future: `current_rtp_time` then answers nothing and the
sender report carries RTP time 0, and a sound frame dated 20 ms ahead is in
the future for most of the 20 to 40 ms until the next frame. The same
difference is written the other way: the picture is dated that much before it
arrived, the sound when it arrived. `tests/sound_lag.rs` reads it back from
the wire (a client dates, by each track's own sender report, a picture and a
sound frame the camera delivered at the same instant: 0.0 ms apart without
the key, 30 at 30, 40 when 200 is asked, 0 with lip sync off).

*What the browser adds, which chose the value.* Edge does not synchronise
exactly: libwebrtc stops moving the picture once the two are within 30 ms of
each other, and since the picture starts ahead it stops with the sound still
behind. Measured on L01 with nothing dated late, the sound is 7 to 9 ms
behind the picture by the browser's own account of the two tracks' dates
(MediaMTX: 12 ms; `sound_behind_ms`). That remainder adds to the lag, and
not evenly: the lag also changes where the browser stops. L01, oxrtc alone,
14 runs of 120 s in alternation (sessions `w1c-sweep-<lag>-<n>` and the
baseline's, 2026-11-20, the owner's NVR running on the same PC). "By the
dates" is the browser's account with the lag added back; "content" is the
beep heard minus the flash seen, whose constant is the measuring path's, so
it is read against the mean of the runs at 0 (86.8 ms; they read 70.6 to
94.9); the latency is the run's mean:

| lag (ms) | latency (ms) | sound behind, by the dates (ms) | sound behind, content against 0 (ms) |
|---|---|---|---|
| 0 | 105.4, 84.7, 92.3, 104.6 | 7, 8, 9, 8 | the zero |
| 20 | 94.4, 80.2, 79.2 | 24, 36, 28 | +23, +25, +30 |
| 25 | 72.2, 86.4, 67.1 | 38, 38, 38 | +47, +32, +29 |
| 30 | 59.7, 59.6, 68.8 | 45, 50, 43 | +58, +55, +48 |
| 40 | 64.1, 49.7 | 49, 53 | +50, +57 |

At 30 the sound is 43 to 58 ms behind the picture, past the 45 ms the owner's
answer rests on, though only 30 are oxrtc's; at 40 no more, because the
picture cannot lead by more than the sound's path is longer than its own
(the picture was not held at all in one run: 49.7 ms is its own path). At 25
one run of three reads 47. At 20 every run is under it by both figures, 23 to
36 ms, and by the content figure where MediaMTX's own sound is (109.5, 111.7
and 117.0 against MediaMTX's 111.7 in the same hour). So the default is 20,
the low end of what the owner allowed and the highest value measured that
keeps every run inside the 45 ms; the key takes no more than his 40. The
latency follows the lag: 97 ms at 0, 85 at 20, 75 at 25, 63 at 30 (means).
*What it gave (2026-11-20, binary of `c4fd02f`, two sessions of L01, 3 reps
abba against both references, the default mode).* Latency mean 70.0 +/- 11.5
ms against MediaMTX's 104.8 +/- 17.7 (**oxrtc better**) in the first session,
95.4 +/- 47.2 against 110.2 +/- 24.1 (no difference) in the second; P95 96.7
+/- 21.7 against 133.6 +/- 21.7, then 115.0 +/- 47.0 against 133.0 +/- 37.9:
no difference in both. oxrtc is the lower in 5 runs of 6 on both rows; the
sixth began with the browser's sound buffer 104 ms deep, the browser moved
the picture to follow it and left it there once the sound was back at 83
ms, the sound then 20 ms ahead of the picture, inside its own dead band.
By the content, oxrtc's sound is 15 to 23 ms further behind its picture than
with nothing dated late, 7 to 10 ms further than MediaMTX's: about 23 to 31
ms behind, taking the runs at 0 as the browser's own 8 ms, within IS-191's
45; go2rtc's sound plays 0.2 s ahead of its picture on the same figure (-211
and -250 ms against oxrtc's 110 and 102). CPU tied with oxrtc lowest, memory
won. So the dating wins what the owner's 45 ms leave: in lip sync the
picture's latency is the sound's path minus how far the sound is behind, the
sound's path is the browser's own buffer (43 to 104 ms of oxrtc's sound in
these runs, wandering 30 ms between runs for either server), and 20 ms over
MediaMTX's own dating does not separate the intervals in every session. A
larger lag is not taken: it would cross the 45 ms (section above). The rows
and what is owed are in the roadmap's W1c.

*Review (2026-11-20, a third L01 session `rv-w1c-accept-L01`, same binary).*
Latency mean 78.5 +/- 12.8 against MediaMTX's 106.5 +/- 38.6,
P95 102.3 +/- 35.7 against 120.1 +/- 40.6: no difference on both, oxrtc the
lower in every repetition; CPU tied (oxrtc lowest), memory won. The choice of
20 stands, but not for the reason written above: "at 20 every run is under
it by both figures" is not what the runs say. The content figure scatters by
up to 30 ms from a session's mean (the runs at 0 read 70.6 to 105.7), and at
20 the first acceptance session itself had a run at +51 against the runs at
0, the third session +20, +31 and +42 (against its own runs at 0, 100.2 and
105.7 ms); by the dates with the lag added back, 29 to 40. What holds is the
MEAN of a session: +15 to +23 at 20 (about 23 to 31 behind with the
browser's 8), +36 at 25 (about 44, at the edge), +54 at 30. So 20 is the
highest value whose session mean stays clearly inside the 45 ms, which is the
reading the acceptance already used. In openIVR's layout, where the viewer's
sound is the camera's transcoded to Opus by FFmpeg (`camera_N_rec`), the
transcode does not put the sound further behind on the bench's camera: beep -
flash 103.5 and 94.0 ms at 0 against L01's 100.2 and 105.7 in the same hour,
114.3 and 109.1 at 20 (sessions `rv-lx1-*`, a local scenario of the review,
L01's camera with `layout: openivr`, one run each). go2rtc in that layout
does not hold its picture for the sound (one run: picture 36.1 ms, beep -
flash 167.6, its sound some 70 ms further behind its picture than oxrtc's at
0: past the 45 ms), where in L01's
plain layout it held it 0.5 s: what the real case (W6) measures is that
layout, not L01's.

**The owner's answer, 2026-11-20: "Chaque mode contre son pareil"** (each mode
against its peer). A latency row compares oxrtc in the mode of the server it
is set against: against go2rtc, which does not hold its picture for the sound
in openIVR's layout (above), oxrtc in live mode (`webrtc.lip_sync: false`);
against the server that synchronises - MediaMTX where it can run the layout,
L01 where it cannot - oxrtc's default, lip sync on. openIVR's default stays
lip sync on, and the real case (W6) runs oxrtc in both modes. Every row of the
scoreboard names the oxrtc mode its runs used (`bench/tables.py`, the CSV's
`oxrtc mode` column, `bench/scoreboard.py`), and no row is won by comparing one
mode against the other mode's peer.

## D31 - The API closes a connection in two steps (2026-11-17)

**The defect.** hyper answers a request as soon as its route has an answer,
and closes the connection when the request's body was not read to its end. A
refusal is decided on the head alone (`401`, `404`, `415`, `400`...), so a
client that sends its body after its head - a second write, a second segment -
was still writing when the socket closed. The kernel answers those bytes with
a reset, and a reset that reaches the client before it has read the answer
destroys the answer: always on Windows, on Linux when the reset comes before
the end of the answer. The client saw a broken connection where a status had
been sent. CI's Windows job failed on it (`e2e.rs`: the `DELETE` and the
`PATCH` of an ended session, each with a body, os error 10053); on the
unfixed listener the test of the mechanism loses the answer three runs in
three. A review had met it once and fixed the one route it met it on
(`PATCH` read its body before refusing, `6a0a097`).

**Decision.** The listener closes every connection in two steps, as RFC 9112
(9.6) describes (`crates/api/src/linger.rs`): the write side ends, so the
client reads the answer and then its end; what the client still sends is read
and dropped until it closes, `LINGER_TIMEOUT` (2 s) passes or `LINGER_BYTES`
(256 KiB, four bodies of the largest size the API takes) were dropped; then
the socket closes. The connection keeps its two slots meanwhile (the total
and its address's, D9), and a stopping server ends every lingering connection
at once. `ServeLimits::linger` is the bound; it is not a configuration key.
One mechanism for every route: `PATCH` refuses on its head again.

**Why.** The alternative was to read every body before any refusal (a
middleware in front of the authentication): 64 KiB buffered for whoever
knocks without a credential, and nothing for what hyper refuses by itself
(a head it cannot parse, a body over the cap). Two seconds are Apache's;
they add nothing an idle peer could not already take (75 s).

Changes if: a client is found that needs longer than 2 s to finish a body the
API refuses (a slow uplink sending 64 KiB), or `axum::serve` gains a
lingering close of its own.

## D32 - FFmpeg 6.1 to 8.0: three release lines, each a job of the CI (2026-11-17)

Until now every test ran with FFmpeg 8.0.1 on one Windows PC, and CI's Linux
job, the only one with another FFmpeg, was red and unread. The owner's
Raspberry Pi 5 runs Debian 13, whose FFmpeg is 7.1.

**Decision.** oxrtc supports the FFmpeg of the systems it is installed on,
and a release line is supported when a job of `ci.yml` runs the whole suite
with it: **6.1** (Ubuntu 24.04's, 6.1.1, on the runner), **7.1** (Debian 13's,
7.1.5 today, in a `debian:trixie` container - image and packages unpinned on
purpose, the job follows what a Pi gets from its distribution), **8.0**
(8.0.1 on Windows). `docs/COMPATIBILITY.md` ("FFmpeg") is the list, with what
was run. Both directions are meant: FFmpeg as a reader of oxrtc's RTSP server
(openIVR's recorder, analysis and probes) and FFmpeg as oxrtc's own child
(the `ffmpeg:` sources, the snapshots, confined by `oxrtc-exec` on Linux).

What the first green run settled: **no defect of oxrtc with FFmpeg 6.1 or
7.1**. The seven "0 frames" of the Linux job were the tests reading a key
(`frame=` of `-progress` with `-c copy`) that FFmpeg 6.1 does not print for a
copied stream; by hand, FFmpeg 6.1.1 and 7.1.5 read 76 packets in 3 s from
oxrtc by every other count. A test counts copied frames from the `framecrc`
muxer now.

A line older than 6.1 (Debian 12's 5.1, Ubuntu 22.04's 4.4) is not supported:
not refused, not tested. Changes if: an installation needs one (a job is added
first, then the claim), or a new FFmpeg line appears on a system oxrtc is
installed on.

Added in review the same day: the Debian 13 job also runs on an arm64 runner
(`test (ubuntu-24.04-arm, debian:trixie)`). The three jobs above are x86-64,
and the installation this decision was taken for is a Raspberry Pi; an
emulator could not stand in for it (qemu-user has no Landlock, and FFmpeg
under the file-size limit dies there: COMPATIBILITY, "FFmpeg"). An
architecture is supported the way an FFmpeg line is: when a job runs the
suite on it.

Also added in review (2026-11-18): "runs the whole suite with it" is checked,
not read off a green job. A test that needs FFmpeg, ffprobe, go2rtc or the
built `oxrtc-exec` skips itself, and passes, on a machine without them, and
cargo shows nothing a passing test printed; `ci.yml` sets `OXRTC_NO_SKIP`,
under which a skip is a failure, so a job that is green ran every such test
with its FFmpeg (`scripts/tests/test_workflows.py` holds the workflow and the
tests to it).

## D33 - A release is published by `release.yml` on its tag; a developer's machine is the fallback, never a second publisher (2026-11-17)

D17 gave two ways to build the same assets: `scripts/release_local.py` on a
developer's machine, and `release.yml` running that script per platform. The
three releases so far (0.1.0 to 0.1.2) were built and published from the
owner's PC, because the workflow never succeeded: the account's quota stopped
it on the first tag, and on the next two cargo-about, run offline, did not
find the crates of the platforms the runner had not built for (a fresh
registry; the PC's held everything). The script fetches the whole lock file
first now (`cargo fetch --locked`), and a manual run of the workflow builds
the three platforms.

Two publishers for one tag is the danger: the archives of the two builds are
not the same bytes (gcc on Ubuntu 22.04 against zig from Windows), and
openIVR's installer pins the SHA-256 of each archive it downloads.

**Decision.**

1. **A release is made by pushing its tag** (`vX.Y.Z` on the commit that
   bumps the version): `release.yml` builds the three platforms, writes
   `SHA256SUMS` over every asset and creates the GitHub release. Nobody
   uploads an asset by hand.
2. **`scripts/release_local.py` publishes nothing.** It is the dry run before
   a tag (`--out`, any commit), and the fallback when Actions cannot run
   (quota, outage): then, and only then, its assets are published by hand
   with `gh release create <tag> <files>`, once the run of the tag is known
   to have failed or not started.
3. **The two never both publish.** The workflow's first job asks whether the
   tag already has a release; if it has, nothing is built and nothing is
   published (a notice says so, the run is green). `gh release create` is the
   only publishing command there: it refuses an existing release, and no step
   replaces an asset. A release's assets never change after it is created; a
   wrong release is superseded by the next version, not repaired.

Why the workflow and not the PC: a public project's binaries come from a log
anyone can read, built from the tag on clean machines, natively on each
system (no cross toolchain between Windows and the Linux archives), and the
release no longer depends on one machine being available and clean.

Changes if: Actions becomes unavailable for the account again (the fallback
becomes the way, and this decision says so), or the two builds are made
reproducible bit for bit (then a release could be checked by building it
twice, and the question of who publishes stops mattering).


## D34 - A source's silence is judged from its own cadence, and a source that dropped is asked again twice a second (2026-11-18)

The bench's R01 and R02 (2026-11-17) lost the recovery after a 10 s outage:
7.1 s from the camera's return to its picture, go2rtc 5.4 s. The timeline of
one run, from oxrtc's debug log (W2): the bench stops the camera's publisher
and keeps the camera server up, so oxrtc's session stays open and goes silent;
"no media within the stall timeout" came 15 s after the silence began, 5 s
after the camera was back; then the first reconnection waited 1 s for the
dead session's TEARDOWN (D25), and the picture came at the publisher's next
keyframe. After 20 s and 60 s, the 404s to DESCRIBE during the outage were
spaced by the backoff (0.6, 1.1, 2.2, 3.8, 5 s): one attempt landed 80 ms
before the camera was back, the next 3.8 s after it. go2rtc's 5.4 s is the
same 15 s read timeout (`#timeout=15`) minus the outage, then its 1 s retry.

1. **The silence of a source is judged from its own cadence**
   (`crates/core/src/cadence.rs`). The channel records the gap between every
   two frames of a connection (the gap across a reconnection is not one), and
   the watchdog restarts a source silent for `SILENCE_FACTOR` (4) times the
   **longest** gap it has ever shown, at least `Limits::silence_floor` (10 s),
   at most its stall timeout (`#timeout=N` or 15 s), which also holds until
   8 gaps spanning 10 s have been seen. The error says so (here with the first 5 s floor): `no media for
   5064 ms, beyond the 5000 ms its cadence allows (longest gap 53 ms)`.
2. **The floor is measured, not guessed.** It is 10 s, raised from the 5 s
   first chosen (review of W2, 2026-11-18): the owner's reference camera
   (.149) stops sending for a few seconds now and then, on both its streams
   at once, and resumes by itself with what it held back - 4.4 s measured on
   2026-11-11, and at 5 s go2rtc sometimes lost patience first, reopened the
   sessions of a camera that rations them, was refused, and one short freeze
   became ten minutes of warnings (openIVR `app/services/streaming/patience.py`,
   which is why openIVR writes `#timeout=15` on every camera). The 5 s floor
   rested on the gaps below, 27 five-minute evening runs that never caught
   such a pause; it would have cut that camera at its first pause after each
   start of oxrtc (the longest gap is learned from zero). 10 s is more than
   twice the measured pause and still the moment the bench's 10 s outage ends.
   The gaps the first floor rested on: the owner's cameras, in the 27
   five-minute runs of RC1 (2026-11-17, every picture a browser presented):
   the two XM cameras (14 pictures a second, in the evening) 0.5 to 1.9 s at
   the longest; the HTTPS webcam (one picture a second) 2.0 s, a picture it
   skipped; the two MJPEG webcams 0.1 to 0.4 s. Read again on 2026-11-18 as
   oxrtc receives them (6 minutes, FFmpeg's arrival times): camera_4 1.18 s,
   camera_5 0.12 s, camera_11 0.14 s. A camera whose rate drops to one picture
   a second at night shows 1 s gaps, within the floor; the longest gap is kept
   for good, so a camera that once paused 3 s may pause 12 s from then on.
3. **A source that reads this server is not judged so** (`local()`: an FFmpeg
   of a stream of ours): its silence is its input's, which is judged where it
   is read; restarting the FFmpeg would only make it probe again once the
   camera is back. It keeps its stall timeout.
4. **A source that dropped is asked again on a short, constant cadence**
   (`crates/core/src/retry.rs`): `Limits::quick_retry` (500 ms, jittered down
   to 375 ms so that a switch's hundred cameras do not ask on one tick) for
   `quick_retry_for` (2 minutes: a camera's or a PoE switch's reboot) after a
   **healthy** run (one that delivered for `healthy_after`, 10 s) ended, while
   each attempt fails in under `cheap_attempt` (2 s: a refused connection, a
   404 from a relay with no publisher). Only a driver whose attempt is a
   connection (`SourceDriver::cheap_attempts`: RTSP and HTTP clients) - an
   FFmpeg run is a process start and keeps the backoff. Refused credentials
   keep their slow backoff (30 s to 10 min), and a source declined before it
   ever delivered keeps the declined one. A connection that timed out is not
   repeated so often: after 2 minutes, or for a slow failure, the backoff
   (0.5 s to 5 s) as before.
5. **A peer that answers and refuses is never asked twice a second** (review
   of W2, 2026-11-18). An RTSP status other than 401 and 404 (453, 461, a 5XX)
   and an HTTP status other than 404 (403, 429, a 5XX) is `SourceError::Busy`,
   retried on the backoff: the owner's XM cameras answer 461 to a session past
   their limit and cut after it (2026-11-05), and as the code first stood that
   461 was a `Protocol` error, asked twice a second for two minutes after
   every drop. 404 keeps the short cadence: it is what go2rtc, MediaMTX and
   oxrtc answer while a relayed camera is away (the bench's outage). And only
   a healthy run opens the two minutes: as first written, every run that
   delivered reopened them, so a source that plays a few pictures and drops at
   each session (a camera shedding a session it cannot keep) was asked twice
   a second for ever - 212 whole sessions in two minutes and 306 in the next
   three on the fake driver, where the backoff asks 29 and 38
   (`recovery.rs`, `a_source_that_drops_after_every_few_pictures_...`).

Measured (bench, same rig, `run.recovery_ms`): see the acceptance session in
ROADMAP W2. A smoke run (1 rep, debug logs, w2-after-diag): R01 0.10 s after
10 s, 1.15 s after 60 s; R02 1.08 to 1.33 s for its ten cameras.

The bench's own recovery was corrected first (`0ad73f7`): a frozen viewer asks
for a keyframe every 3 s, oxrtc answers from its cache, and the browser
presents that old keyframe anew, which the old rule counted as the camera back
(HEAD read 2.1 s where the picture came at 7.1 s).

Changes if: a camera is seen cut while healthy (a `no media for ... its
cadence allows` line on a source that was delivering: then the floor or the
factor rises, from that camera's gaps), or a camera is seen refusing or locking
up under two attempts a second after an outage (then `quick_retry` grows for
it, or the cadence becomes per source).

**Accepted by the owner, 2026-11-19**: retrying a camera twice a second for up
to two minutes while each attempt is refused at once is accepted, his XM
cameras included.

## D35 - A keyframe request is left to the camera when its keyframe is near, or when the viewer is behind (2026-11-18)

The bench's B04 and B06 (2026-11-17) lost the browser's rows: with 25
cameras or 50 viewers of one camera in one headless browser, oxrtc's viewers
dropped 1348 and 1428 frames and froze 94 and 90 times in 120 s, go2rtc's
838 and 954, 37 and 41. The browser is the bottleneck for every server: it
decodes about 15 of the camera's 25 frames a second per viewer, falls behind,
drops frames and asks for a keyframe, with nothing lost on the wire (no
packet lost, no NACK, no retransmission in any run). go2rtc and MediaMTX
answer nothing; the viewer resumes at the camera's next keyframe, a second
away on the bench. oxrtc answered each request with the cached group
(R24, D15): a keyframe and up to 24 frames written within one frame
interval, to a browser that asked because it could not keep up - its viewers
received 1.3 to 1.8 times the frames the camera sent (4566 against 2946 per
viewer in B06), dropped them, asked again (160 requests against 73) and were
replayed again, about once a second (W3; probe counters added to the bench
for it).

1. **A request is left to the camera** (`Answer::Camera`: nothing replayed,
   and the session's picture stops until the camera's next keyframe, since
   nothing it would send before it can be decoded) when that keyframe is
   due within `CAMERA_KEYFRAME_SOON` (1 s), from the step between the last
   two keyframes the session saw (`KeyClock`). A viewer of the bench's
   camera is never replayed; one of the owner's cameras (a keyframe every
   4.8 to 6 s) is, except in the last second of a group.
2. **A viewer that asks again within `BEHIND_AFTER_REPLAY` (2 s) of a
   replay is left to the camera too**: the replay did not bring it back, it
   is behind, and another group would be another burst. A request after
   that is a new loss and is replayed.
3. A group whose interval is not known yet (fewer than two keyframes seen)
   is replayed, as before; so is a cut of the session's own queue
   (`Discontinuity`), which is how a late session gets its first picture.

Measured, one at a time, on B06 (oxrtc alone, one repetition each, an
experiment switch never shipped): the replay (HEAD) 1473 frames dropped and
88 freezes; no answer, frames still sent (go2rtc's behaviour) 943 and 44;
no answer and the picture stopped until the keyframe 901 and 43; go2rtc 888
to 920 and 36 to 38 in the same hour. With rule 1 against go2rtc, three
repetitions in A B B A order: B06 916 +/- 15 frames dropped against 912 +/-
17, 44.9 +/- 2.4 freezes against 42.4 +/- 1.5; B04 880 +/- 175 against 839
+/- 77, 38.0 +/- 9.8 against 37.8 +/- 5.5: no difference, where the rows
were lost. The server's CPU on B06 fell from 38 to 20 % of a core (go2rtc
75), the replays having been a third of it.

What was tried for the rest and refused: go2rtc's remaining edge on freezes
comes from its sound (without the camera's sound go2rtc froze 43.6 times and
oxrtc 42.4): it sends G.711 in 128 ms packets, so the browser's sound buffer,
and the picture held for it, are deeper - the same packets that cost it its
435 ms of latency (W1). A playout-delay floor on every picture (100, 200,
400 ms) did not deepen the browser's buffer under that load (13 to 24 ms)
nor reduce the freezes (42, 47, 43); G.711 joined into 40 and 120 ms packets
made no difference outside the run-to-run spread (42.9 and 41.4 against
go2rtc's 41.7 in the same minutes). Pacing (each frame's packets spread) was
not the cause either: no packet was lost, and go2rtc sends a frame's packets
at once as oxrtc does. The RTP times a viewer receives follow the source's
own steps within a run (D12), not the arrival at oxrtc.

Tried after the acceptance run and withdrawn (`ee24635`, reverted by
`663fce7`): B05 (100 viewers in one browser) stayed lost - 4.98 frames
presented a second against 6.82 (go2rtc) and 9.55 (MediaMTX), 1791 dropped
against 721 and 469 (3 reps abba). The references, at 160 % of a core, send
unevenly, so the browser's jitter estimate - how long it holds each picture
- grows to 229 ms (go2rtc) and 628 ms (MediaMTX), and that slack lets a
decoder that falls behind keep its frames; oxrtc sends evenly, the browser
holds 8 ms, a late frame is dropped, the chain is broken until the next
keyframe and the decoder idles (511 frames decoded a second in that browser
against 990 for MediaMTX). A playout-delay floor per session, raised 100 ms
a second while its viewer kept asking for keyframes (up to 600 ms, lip sync
on only), was honoured as a 207 ms hold and gave B05 961 frames dropped
(won against go2rtc's 1275) and 6.28 presented a second, still behind
MediaMTX's 462 and 9.58; and it made B04's freezes a LOST row (47.0 +/- 7.0
against 36.7 and 35.8, 37.8 without the floor), so it was withdrawn. B05
against MediaMTX stays open (roadmap W3).

The cost: a viewer that asked for a keyframe near the end of a group waits up
to a second for the camera's, as go2rtc's and MediaMTX's always do (on a
camera with a 6 s group, up to 6 s, and only when it asked again within 2 s
of a replay). Nothing in the configuration: the two bounds are constants.

Corrected by the review (2026-11-19): rule 1 trusted the last step between
two keyframes for the whole next group, and a keyframe that did not come on
it read "due now" until one came. A step can be short when the next group
is not - a source that restarted mid-group sends its first keyframe early,
x264 (openIVR's `ffmpeg.h264`, scene cuts on) adds one at a change of scene,
a camera's groups vary - and every request of that group was then left to a
keyframe up to a whole group away, the picture stopped and the browser
asking again meanwhile (on the wire: keyframes 1 s apart then a 22 s
group, a request 2 s into it got nothing in the 1.2 s observed, where a
replay answers within the 800 ms the test allows). Now a keyframe late by
more than a quarter of its step (`KEYFRAME_LATE`) makes the interval
unknown again, and the request is replayed as rule 3 says. The bench's camera (a keyframe every 25 frames,
exactly) is never late, so none of the rows above moves.

**B05 and B06 are closed as stress rows (the owner, 2026-11-19; D29 as
amended).** The real deployment has two to five viewers at most, each in a
browser of its own; 100 viewers in one browser (B05) and 50 (B06) do not
happen. Both stay in the bench, are reported and are never made worse, but
they are outside the count of rows to win, and no server-side trick that
costs latency or lip sync in the real case (the playout floor above was one)
is taken to win them. B05 against MediaMTX is therefore no longer what keeps
W3 open; the real case (five browsers on openIVR's live grid) is W6's
scenario.

## D36 - The transcoder's cost and life: what a new viewer waits for, what FFmpeg runs beside it (2026-11-19)

The bench's real cameras (RC1 to RC4, 2026-11-17) lost the first frame of the
owner's two MJPEG webcams through openIVR's H.264 transcode (RC2 warm: 2.3 s
and 2.4 s against go2rtc's 0.9 and 1.0 s, MediaMTX's 0.7 and 0.8 s), the
FFmpeg children's memory (RC1: 616 MiB against 476 for go2rtc) and the tree's
cost with nothing held (RC3: 20.9 % of a core against 6.7 %). Measured again
with 0.2.0 (`w4-base`, one repetition, oxrtc and go2rtc) before anything
changed: RC2 camera_5 and camera_11 2.8 and 2.9 s, RC1's children 717 MiB
against 569 (W4).

1. **A source declined by its input and waiting out its backoff is passed
   over by a new consumer** (`Slot::declined_and_waiting`, in `select.rs`
   `first_offer`). Read from a debug run of RC2 (`w4-diag-rc2`), camera_5's
   viewer offers at 06:39:19.559: the transcode starts at .563 and publishes
   at 20.291 (0.73 s), the answer goes at 22.508 - the sound transcode of the
   webcam (openIVR's `camera_5_rec?audio=opus`; the camera has no microphone,
   the relay answers its FFmpeg `455`) had been declined at 17.816 and slept
   its declined backoff (4.6 s) until 22.401, was declined again at .439 in
   38 ms, and only then let go. The next two viewers waited for the next
   sleeps the same way (5.38 s, then 8.01 s: the probe deadline, the backoff
   having reached 19 s), because the route was let go a few milliseconds after
   the supervisor had looked for demand, so the source was never retired
   while viewers came. A consumer took the sound on its word (it declares
   Opus) and waited for an attempt that its backoff had put off. Now a source
   whose last attempt was declined and that is in `Retrying` is not a
   candidate: the consumer goes on with its other media at once, as it would
   after the decline. A source that is idle again (retired) is tried once by
   the next consumer, as R47 left it: a decline takes about 40 ms. RC2 on the
   fixed build (`w4-v1-rc2`, one repetition): camera_5 340 ms, camera_11
   379 ms (warm transcode 80-185 ms, cold 0.8-0.97 s).
2. **FFmpeg runs with no console** (`crates/ffmpeg/src/console.rs`):
   `DETACHED_PROCESS` where `CREATE_NO_WINDOW` was. A console program started
   with `CREATE_NO_WINDOW` gets a console of its own without a window, and a
   console is a `conhost.exe` (7.0 MiB): RC1 counted eight beside oxrtc's
   eight FFmpegs, 56 MiB, none in go2rtc's tree (its children share its
   console). Measured by hand with the real FFmpeg and Chocolatey's shim, the
   shim detached gives the FFmpeg it starts a new console WITH a visible
   window (EnumWindows), so a launcher keeps `CREATE_NO_WINDOW`: a binary is
   detached only when its path is absolute and not a launcher's
   (`chocolatey\bin`, Scoop's `shims`, a `<name>.shim` beside it). A bare
   `ffmpeg` (go2rtc's default, which `PATH` may resolve to a shim) keeps its
   hidden console and its host. openIVR writes the real binary's path.
3. **The relay's transcodes do not guess the frame rate; a slow webcam's is
   announced** (D27's "changes if": the rate carried in the relay's SDP).
   An input that reads the relay starts with `-fpsprobesize 0`
   (`crates/ffmpeg/src/args.rs` `RELAY_INPUT`), and the `DESCRIBE` of a
   Motion JPEG track says `a=framerate` (RFC 8866) when its pictures come
   every 100 ms or slower (`sdp.rs` `ANNOUNCED_INTERVAL`; the pace is the
   channel's `PictureRate`, a running mean of the gaps between the video
   track's pictures, the newest an eighth, known after two gaps and kept
   across the source's connections). Measured before deciding, each on the
   real path (oxrtc of the tree, real FFmpeg 8.0.1, the transcode read back
   over RTSP), one session at a time on the owner's webcams:

   | source | FFmpeg's guess (today) | output rate | resident | first picture |
   |---|---|---|---|---|
   | camera_5, 30/s | 20 pictures, finds none | `90k tbr` | 100 MiB | 0.75-1.2 s |
   | the same, `-fpsprobesize 0` | - | `90k tbr`, frames the same size | 72 | 0.12-0.30 s |
   | camera_11, 25/s | finds none | `90k tbr` | 102 | 1.36 s |
   | camera_4, 1/s | 20 s, finds `1 fps` | `1 fps`, 353 KB a frame | 107 | 21.2 s |
   | the same, `-fpsprobesize 0` alone | - | **`90k tbr`, 50 KB a frame** | 72 | 0.56 s |
   | the same, with `a=framerate` (warm) | - | `1 fps`, 267 KB | 73 | **0.07 s** |
   | camera_12 (ESP32), about 8/s | finds `25 fps` | `25 fps` | 106 | 2.2 s |
   | the bench's H.265 2304x1296, 100/7 | from the parameter sets | `14.25 fps`, the same bytes | 126 -> 106 | 2.6 -> 1.3 s |

   `-fpsprobesize 0` alone would have changed the 1 fps webcam's output to
   x264 told 90000 pictures a second (the quality loss D27 refused); a rate
   announced for every webcam would have changed the fast ones' (x264 told
   their real 25 or 30 where it is told 90000 today, as in front of go2rtc,
   whose FFmpeg reads the same guess): more bits and about a sixth more CPU,
   a change of the operator's encoding that this item does not make. So the
   rate is announced only where FFmpeg's guess found one. Tried and refused:
   `-fpsprobesize 3` (camera_5 read 47.95 fps and camera_11 16 fps from three
   arrival-dated pictures, and the output's timestamps were put on that
   wrong grid: 486 pictures in 20 s dated 30 s apart), `-fflags nobuffer`
   (the same 28 MiB less, but the first picture later: D11), `-threads 1`
   and `-thread_type slice` (nothing). Not done: a webcam's very first
   transcode after oxrtc starts, before two gaps of its pictures were seen,
   has no rate to announce (camera_4: x264 at 90000 for that viewing); in
   openIVR's layout the recorder holds every camera from the start, so its
   pace is known before any viewer. `first_track_timeout` (28 s) stays.
   **FFmpeg 8.0 and later only** (found by the CI of `b83266a`: the wire test
   read `avg_frame_rate=0/0` with 6.1.1 and 7.1.5): FFmpeg reads
   `a=framerate` from an RTSP description since 8.0 (and writes it since
   then too; checked in a `debian:trixie` container, 7.1.5 on an SDP saying
   `a=framerate:2` read `0/0`). With an older FFmpeg the one-picture-a-second
   webcam would be encoded as if it sent 90000, so `-fpsprobesize 0` is
   given only to an FFmpeg of 8 or later, asked once with `ffmpeg -version`
   when the first transcode starts (`crates/ffmpeg/src/version.rs`; a
   development build `N-...` counts as new, an unreadable answer as old).
   With 6.1 and 7.1 (Ubuntu 24.04, Debian 13: what a Raspberry Pi installs)
   the transcodes keep FFmpeg's guess and its wait, as before; the
   announcement is harmless there.
4. **A transcode stops with its last consumer; a camera keeps its grace**
   (`SourceDriver::transcodes`, `Limits::transcode_idle_grace`, 0; the keys
   `api.idle_grace_ms`, default 5000, and `api.transcode_idle_grace_ms`,
   default 0, each 0 to an hour). One grace for every source kept each of
   openIVR's transcodes, a core and 70 to 110 MiB, 5 s after its viewer, for
   a viewer who may not come. With 1 and 2 above, a transcode of a camera the
   recorder holds starts in 0.1 to 0.3 s, so the grace bought little. Decided
   on one repetition of each, oxrtc alone, same build (`w4-v2` with 5 s,
   `w4-g0` with 0; go2rtc from `w4-base`):

   | row | go2rtc | grace 5 s | grace 0 |
   |---|---|---|---|
   | RC2 warm first frame, every attempt | 6199 ms | 169 | 223 |
   | RC2 webcams (camera_5, camera_11) | 889, 1339 | 158, 221 | 201, 205 |
   | RC2 XM (camera_1, camera_2) | 5855, 6786 | 152, 147 | 262, 235 |
   | RC2 tree CPU / RSS | 65.4 % / 141 MiB | 56.2 / **163** | **26.4 / 90** |
   | RC3 tree CPU / RSS | 6.3 % / 74 MiB | **17.9** / 62 | **4.7 / 29** |
   | RC3 cold first frame, every attempt | 8497 ms | 951 | 1042 |

   The grace cost RC3's CPU and RC2's memory, both lost to go2rtc, for 50 to
   110 ms of a first frame that is won either way (MediaMTX, whose
   `runOnDemandCloseAfter` keeps a transcode 10 s, read 0.66 to 1.35 s in
   the session of 2026-11-17). A camera's session stays: it is what makes a
   transcode start warm, and it costs a connection, not a core.

**Reviewed, 2026-11-19 (W4 review).** Three corrections, each proven before
it was made:

- **The pace is a median, not a running mean** (item 3). The mean, the newest
  gap an eighth, let ONE pause of a fast webcam on its connection (a hiccup
  of an internet MJPEG camera, shorter than its silence, so no reconnection)
  read as its pace: on the real path (oxrtc of the tree, a local MJPEG
  publisher at 30 pictures a second pausing 3 s, a raw `DESCRIBE` of the
  relay every 100 ms), four `DESCRIBE`s in the 0.4 s after the pause said
  `a=framerate:2.466` to `6.868`, and a transcode started just after a pause
  was told it for its whole life (its output read `avg_frame_rate=41/6`
  where every other start reads FFmpeg's `90000/1`). `PictureRate` is now
  the median of the newest 15 gaps (half a second at 30 a second, 15 s at
  one): 0 of 74 `DESCRIBE`s announce a rate around the same pause, the
  transcode reads `90000/1`, and the one-picture-a-second source is still
  announced at every `DESCRIBE` (`0.999`, 73 of 73, the pause included).
- **Only an FFmpeg that encodes is a transcode** (item 4).
  `SourceDriver::transcodes` was true for every `ffmpeg:` source, so a camera
  read through FFmpeg that only copies (`#video=copy`, or no codec: `-c
  copy`) lost the camera's 5 s grace that this decision keeps. It is true
  for a `#video=`/`#audio=` other than `copy`, a `#raw=` or a filter
  (`Plan::transcodes`); openIVR's layout has no copy-only `ffmpeg:` source,
  so its rows are unchanged.
- **A coded camera's pace is announced too, and the transcode's answer waits
  for it** (item 3). "The output is the same" was measured on the webcams
  and on the bench's H.265 camera, whose encoder writes its rate in the
  parameter sets. The owner's two H.265 cameras write none: through his
  running relay, ffprobe 8.0.1 reads `r_frame_rate=100/7` (camera_1) and
  `57/4` (camera_2) with FFmpeg's guess and `90000/1` with `-fpsprobesize
  0`. So since item 3 x264 was told 90000 pictures a second for the cameras
  his browsers watch through this transcode, where 0.2.0 and go2rtc's FFmpeg
  tell it 14.3, and with a constant rate factor x264 scales its quantizer
  with the picture's duration. On the real path (an oxrtc of its own reading
  one session of his relay's `camera_1_src`, his `h264` template, the
  transcode read 20 s; builds alternated, 286 or 287 pictures each):

  | build | kbit/s | mean picture | inter pictures |
  |---|---|---|---|
  | 0.2.0 | 7209, 7003 | 62641, 60847 B | 36016, 33995 B |
  | item 3 as it was | **3824, 3895** | 33239, 34096 | **12849, 11091** |
  | corrected, an hour later | 6055, 5863 | 52796, 51119 | 19834, 16343 |
  | 0.2.0, between those two | 6019 | 52611 | 16988 |
  | corrected, nothing holding the camera | 6070 | 52926 | 18329 |
  | item 3 as it was, nothing holding | **3783** | 33001 | 12927 |

  Half the bits, on the same scene: the change of the operator's encoding
  this decision refuses, which no row of the bench sees (none measures the
  picture) and which made RC1's FFmpeg CPU of those cameras look better
  than it was. Reading the transcode's output with ffprobe does not show it
  either: it guesses the output's rate from its timestamps again. Now:
  1. `PictureRate` reads the pace from the pictures' own dates (their
     timestamps on the hub's timeline), not from their arrival: FFmpeg's
     guess, which the announcement replaces, reads the timestamps, and a
     camera read through a relay that replays its group of pictures (the
     measurement's own chain) or one that sends in bursts delivers a second
     of pictures in a millisecond (read by arrival: `1 µs`). B pictures,
     dated before the picture they follow, share the gap that picture
     closes. A webcam over HTTP has no clock of its own and is dated as it
     arrives, as before. A pace is told after three gaps, not two: one odd
     gap of the first two was half of them.
  2. The `DESCRIBE` of a coded video track (H.264, H.265) says
     `a=framerate` whatever its pace (`14.286` for the owner's cameras); a
     Motion JPEG track as before, ten a second or slower.
  3. A reader that says it is one of this server's transcodes
     (`source=ffmpeg:` in its query, which the `ffmpeg:` driver writes for a
     stream it reads by name, as go2rtc does) has its `DESCRIBE` held until
     the pace of the pictures is known, 4 s at most
     (`crates/rtsp-server/src/pace.rs`): three gaps after the source's first
     picture ever - 0.2 s at 14 a second, 3 s at one - where FFmpeg's guess
     read twenty; the pace is kept from then on, across connections and
     idle periods. Every other reader is answered at once, as before. This
     closes the "not done" of item 3: a camera's very first transcode, with
     nothing holding it, is told its rate too (the last two lines of the
     table), which is every transcode of a server that holds nothing, as a
     go2rtc-like installation does. A source that shows no pace in 4 s (one
     slower than a picture every 1.3 s) is answered without one, and its
     first transcode is encoded as 90000 a second; FFmpeg's guess needed
     twenty of its pictures, more than the 30 s it is given to start.
  4. `-fpsprobesize 0` is given only to a stream read by name - the reader
     that says `source=ffmpeg:`. A relay URL the operator wrote
     (`ffmpeg:rtsp://127.0.0.1:8554/x#video=h264`) says nothing of the kind,
     is answered at once and keeps FFmpeg's guess; openIVR's own such
     sources read the sound alone, where there is no rate to guess.

- **A camera that stamps on a grid is announced at the grid's rate** (item
  3). The correction above left "not examined" a Motion JPEG camera with a
  clock of its own, and said none of the owner's cameras was one. His ESP32
  (camera_12) is: it dates its pictures on a 25-a-second grid and fills one
  slot in three to five. Read through his running relay (one ffprobe, 5 s):
  41 pictures in 4.96 s, 80, 120 or 160 ms apart, each date within a
  millisecond of a multiple of 40 ms. FFmpeg's guess reads the grid
  (`r_frame_rate=25/1`), so 0.2.0 and go2rtc have x264 told 25 pictures a
  second; announced at its median pace since item 3 (`a=framerate:8.335`),
  x264 was told 8.3. On the real path (an oxrtc of its own reading one
  session of his relay's `camera_12_src`, his `h264` template, 20 s, builds
  alternated):

  | build | relay says | output | kbit/s | mean picture |
  |---|---|---|---|---|
  | 0.2.0 | nothing | `25/1` | 4825, 4965 | 76043, 75997 B |
  | item 3 with the corrections above | `a=framerate:8.335` | `25/3` | **9139, 9376** | 141315, 141445 |
  | corrected, an hour later | `a=framerate:25.014` | `25/1` | 4097, 4061 | 64934, 64907 |
  | 0.2.0, between those two | nothing | `25/1` | 4166, 3981 | 64860, 65218 |

  86 % more bytes a picture, and the CPU with them: the change of the
  operator's encoding this decision refuses, in the other direction.
  `PictureRate` (now `crates/core/src/picture_rate.rs`) keeps the gaps in
  the track's own clock ticks and knows the grid the dates sit on (`grid`):
  the coarsest step - the smallest gap, or a half, a third ... an eighth of
  it - that every gap is a whole number of, off by less than a tenth of a
  step (root mean square), and coarser than 2 ms (a finer step is a clock's
  resolution, not a frame rate). A step finer than the smallest gap is
  believed from eight gaps on: with three, some fraction fits by chance.
  The `DESCRIBE` says the grid's rate first, whatever the codec (`sdp.rs`
  `framerate`), else the pace as before. What follows from it:
  - a camera that fills every slot is announced as before - the grid is its
    pace (`14.272` for the owner's H.265 cameras, the mean of their steps,
    where the median read `14.286`; `0.999` for the one-a-second webcam);
  - a Motion JPEG camera with a clock of its own that fills every slot of a
    25 or 30 grid, which the ten-a-second rule left with no announcement
    (x264 told 90000), is announced at its rate;
  - a template that encodes at a constant rate (go2rtc's default `h264`,
    unlike openIVR's `-fps_mode passthrough`) is given the rate of the
    slots, not of the pictures that happened to fill them when the
    transcode started: a camera that fills more of them later is not cut
    down to that.

  Not covered: a camera's very first transcode with nothing holding it
  knows three gaps, and a grid finer than those is not believed from three;
  that one start is told the pace. Proven on the dates read from his
  camera: `crates/rtsp-server/tests/jpeg.rs`
  `a_cameras_rate_is_announced_to_ffmpeg_as_its_own_guess_read_it` (the
  relay says 25 and FFmpeg 8 reads it; with the grid taken out it says the
  pace and the test fails) and
  `ffmpegs_own_guess_of_a_camera_that_skips_slots_is_its_grid` (FFmpeg's
  own guess of the same camera: `25/1`).

- **A webcam whose pictures come evenly is announced at its rate, a whole
  one when the dates fit it** (item 3, 2026-11-20). Item 3's table says
  FFmpeg's guess "finds none" for the 25 and 30-a-second webcams, measured
  one morning; it is not a property of the cameras. Read through the
  owner's relay on the evening of 2026-11-19, the guess read
  `r_frame_rate=30/1` for camera_5 and `25/1` for camera_11, and 0.2.0's
  transcode of camera_5 then carried 7.4 Mbit/s where item 3's, told no
  rate, always carried 2.9: FFmpeg's guess finds a webcam's rate when its
  pictures come evenly enough that minute (each date within a tenth of a
  slot, as a standard deviation), and none otherwise. So "the rate is
  announced only where FFmpeg's guess found one" has to be read from the
  dates, not from the camera's speed. The grid is now judged as that guess
  judges it: the dates, from the first, against the slots of the step
  (their variance under a hundredth of a slot squared), where the earlier
  correction compared each gap with the smallest. Two things the real
  camera said that a synthetic one did not (camera_5 read directly, his
  relay being down, one session, 60 s of dates on an even and on an uneven
  minute, each window of 21 dates given to both rules, FFmpeg's written
  again from its description):
  1. Counting the slots from the span and the median gap misread it one
     start in ten: most of its pictures come 34 to 36 ms apart and one in
     five 27 ms after the last, so fifteen gaps span 14.5 median ones. Each
     gap is now the whole number of steps nearest to it. On the same dates
     the relay's rule and FFmpeg's then agree on 246 windows of 254 on the
     even minute (FFmpeg alone finds a rate in 7, the relay alone in 1) and
     232 of 254 on the uneven one (17 and 5), where the first cut left 22
     and 26 to FFmpeg alone.
  2. A step measured over half a second of arrival dates is good to a
     hundredth: the relay said `29.421` to `30.323` for a camera that sends
     30, and a template that encodes at a constant rate would have dropped
     or repeated a picture every two seconds. The whole number of pictures a
     second nearest to the step measured is said when the dates fit it no
     more than twice as loosely (`WHOLE_RATE_SPREAD`): `30.000`, `25.000`,
     `1.000` on his webcams; a camera whose own clock is no whole rate (100
     pictures in 7 seconds, 30000 in 1001) fits its own step exactly and is
     told as it is.
  3. Dates that are only near their slots are believed from a full window
     of fifteen gaps; dates that are on them (a camera's own clock, within
     a thousandth of a slot) from three, as before. A few gaps of arrival
     dates fit some step nearly every time: of the even minute above, three gaps
     read a rate in 97 windows of 100 and it was 28 to 33 two times in
     three, eight gaps were wrong one time in four, fifteen 4 times in 343.
     The reader this matters to is the first transcode of a camera nothing
     holds, answered after three gaps. On the real camera that start was
     told nothing either way (four cold starts of each build, 2.8 to 3.0
     Mbit/s: the first pictures of an HTTP connection come in a burst), so
     this is a guard read from the dates, not a defect seen on the path.
     The ESP32's grid, which the earlier correction believed from eight
     gaps, is now believed from fifteen (3 s of its pictures).

  On the real path (an oxrtc of its own reading the webcam directly, held
  as the recorder holds it, his `h264` template, each transcode a new
  FFmpeg read 12 s; builds one after the other on the one session a webcam
  is allowed, 2026-11-20 07:26 to 07:40, the machine calm):

  | camera | build | relay says | kbit/s |
  |---|---|---|---|
  | camera_5, uneven minutes | 0.2.0 | nothing | 2799, 2683, 2722, 2874 |
  | | corrected | nothing, nothing, `30.000`, nothing | 2870, 2783, **7101**, 2881 |
  | | the grid correction as it was | nothing | 2932, 2943, 2886, 2813 |
  | camera_5, even minutes | corrected | nothing, `30.000` x3 | 2888, **7054, 7156, 7330** |
  | | 0.2.0 | nothing | 7242, 7157, 7152, 7225 |
  | camera_11 | corrected | `25.000` | 9942, 9967, 9925, 9964, 9897, 10043, 9930, 9717 |
  | | 0.2.0, between | nothing | 9979, 9896, 9886, 9767 |
  | camera_4 | corrected | `1.000` | 2427, 2388 (261 and 265 KB a picture) |
  | | 0.2.0 | nothing | 2718 (261 KB a picture) |

  The transcode of a fast webcam is what it was with 0.2.0 and what it is
  in front of go2rtc: one of two encodings, by the evenness of the half
  second in which it starts. That is FFmpeg's guess's own behaviour and
  this item does not change it; whether a webcam should always be encoded
  at its rate (the 7 Mbit/s one, every start) is the operator's to say - it
  would cost more than go2rtc's on an uneven minute. It also means a CPU
  row of a webcam's FFmpeg compares equal work only when both contenders'
  starts fell on the same side (RC1 and RC4 of camera_5 and camera_11).
  Tests: `picture_rate_tests.rs`
  `a_webcam_that_catches_up_now_and_then_is_read_at_its_rate_not_its_median_gap`
  and `a_webcam_whose_pictures_come_evenly_is_read_at_its_whole_rate` (the
  dates read from his camera; no grid before),
  `a_camera_whose_own_clock_is_no_whole_rate_is_told_as_it_is`;
  `tests/jpeg.rs` (the relay says 30 of those dates and FFmpeg 8 reads it).

- **The answer held for the pace is never held past what its FFmpeg waits**
  (the third correction's item 3, 2026-11-20). That correction holds a
  transcode's `DESCRIBE` up to 4 s after the stream's tracks are known. The
  reader is an FFmpeg with a socket timeout - 5 s by the `timeout` template
  unless the source says `#timeout=` - and the source's own start is part
  of what it has waited. On the real path (oxrtc of the tree, FFmpeg
  8.0.1, a local webcam that answers after 2.5 s and sends a picture every
  2 s, read as `ffmpeg:x_src#video=h264`): the answer came at 6.5 s, FFmpeg
  had left at 5 with `Error opening input: Invalid data found when
  processing input`, and the transcode's reader was refused (`503` after
  5.2 s) where the builder's item 3, with no hold, served it; the same
  words ended `a_viewer_that_starts_a_transcode_cold_gets_its_first_keyframe_first`
  on a Windows runner (CI run 44689818084: the viewer answered `500` after
  6.7 s; whether the hold or the fake camera's cold start alone used up
  FFmpeg's five seconds, that log does not say). The hold is now cut
  to what the reader's timeout leaves, a second kept back
  (`pace.rs` `pace_wait`): 5 s when the source says none, the `#timeout=N`
  the reader's own `source=ffmpeg:...` carries otherwise. The same webcam
  is then served after 4.6 s (its first transcode told no rate: x264 at
  90000 for that viewing, as before the third correction), and with
  `#timeout=20`, as openIVR writes its transcodes, the answer waits the
  whole 4 s as before (8.6 s). A source that takes longer to start than its
  FFmpeg waits still fails its first attempt, as it always did: that is the
  `timeout` the operator sets. Tests: `crates/rtsp-server/tests/pace.rs`
  `a_transcodes_answer_is_never_held_past_what_its_ffmpeg_waits` (6.5 s
  with the hold as it was), `pace.rs`
  `the_wait_is_cut_to_what_the_readers_timeout_leaves`.

- **How long that FFmpeg waits is read where the driver read it** (the
  correction above, 2026-11-20, fifth reviewer). That correction took the
  reader's timeout to be 5 s when its source says no `#timeout=`, and the
  last of two when it says two. Neither is what the driver gives FFmpeg
  (`crates/ffmpeg/src/args.rs` `plan`): the first `#timeout=` of a source,
  else the configuration's `ffmpeg.timeout` template, which is 5 only until
  an operator writes another. On the real path (oxrtc of `01a3c95`, FFmpeg
  8.0.1, `ffmpeg: {timeout: 3}`, a local webcam of a picture every 1.2 s
  read as `ffmpeg:x_src#video=h264`, nothing holding it): the answer was
  held 3.6 s for the pace, FFmpeg had left at 3 with `Error opening input:
  Invalid data found when processing input`, and the reader of the
  transcode was refused (`503` after 3.3 and 3.6 s, two runs), where the
  builder's item 3, with no hold, served it in 1.5 s. The first transcode
  of a slow camera on such a configuration was refused once per start of
  the server (the pace is known to the next attempt). Now the binary hands
  the RTSP server what that template says
  (`oxrtc_ffmpeg::input_timeout`, `RtspServerConfig::transcode_timeout`,
  one reading of the template for both), and `pace.rs` reads the first
  `#timeout=` of the `source=`. The same webcam is served after 2.1 s (told
  no rate, as before the third correction); with the default template
  nothing changes (the rate known and said, `5/6`), and a source that says
  `#timeout=20` under `ffmpeg.timeout: 3` is waited for as before. Tests:
  `pace.rs` `the_readers_timeout_is_the_configurations_when_its_source_says_none`
  (4 s where 2 were left, with the constant) and
  `the_wait_is_cut_to_what_the_readers_timeout_leaves` (the last of two
  read: 7 s for a source whose FFmpeg was given 9);
  `crates/rtsp-server/tests/pace.rs`
  `a_transcodes_answer_is_never_held_past_a_timeout_the_configuration_shortened`;
  `crates/ffmpeg` `templates.rs`
  `of_two_timeouts_the_first_is_the_one_ffmpeg_is_given` and
  `the_input_timeout_told_is_the_one_the_plans_are_given`; `apps/oxrtc`
  `app_tests.rs` `the_rtsp_server_is_told_how_long_the_transcodes_ffmpeg_waits`.

**Accepted, 2026-11-20 (W4 review; acceptance on the reviewed binary).**
`01a3c95`, session `w4-review5-accept`: RC1 to RC4, three repetitions
interleaved A B B A against go2rtc 1.9.15 and MediaMTX 1.21.3, FFmpeg 8.0.1,
the owner's NVR stopped (a calm machine, 7 % before each run, his XM cameras
holding no other session), 35 of 36 runs `ok` (one MediaMTX RC4 run lost its
browser page). Means with their 95 % interval:

| row | go2rtc | oxrtc | MediaMTX | |
|---|---|---|---|---|
| RC2 warm first frame, every attempt | 6117 +/- 215 ms | **303 +/- 23** | 4601 +/- 227 | won |
| RC2 camera_5 / camera_11 | 870, 999 | **283, 283** | 631, 771 | won |
| RC2 tree CPU / memory | 62.6 % / 139 MiB | **28.8 / 87** | 90.0 / 553 | won |
| RC3 cold first frame, every attempt | 6544 +/- 1012 ms | **946 +/- 99** | 8365 +/- 600 | won |
| RC3 camera_5 / camera_11 | 1230, 1539 | **662, 677** | 1154, 1354 | won |
| RC3 tree CPU | 6.5 +/- 0.8 % | **4.5 +/- 1.8** | 42.3 | tied, lowest |
| RC3 tree memory | 66.3 MiB | **30.8** | 243 | won |
| RC1 camera_5 / camera_11 first frame | 867, 986 ms | **392, 444** | 1092, 1281 | won |
| RC1 camera_4 first frame | 21.5 s | **0.99 s** | 30.9 s | won |
| RC1 webcams' FFmpeg CPU (camera_5, camera_11, camera_12) | **66.7, 66.0, 17.3 %** | 70.2, 72.7, 21.7 | 82.9, 77.6, 22.2 | tied behind |
| RC1 children memory | **536 +/- 4 MiB** | 546 +/- 9 | 687 | tied behind |
| RC1 tree memory | 621 MiB | **605** | 760 | won |
| RC1 children CPU / tree CPU | 245 / 348 % | 258 / 275 | **141 / 158** | MediaMTX (less work: W6) |
| RC4 first frame, the cameras that are not XM | 38.8 to 45.6 s | **5.9 to 8.9 s** | 28.1 to 35.6 s | won |
| RC4 camera_5 / camera_11 FFmpeg CPU, run mean | **42.9, 39.0 %** | 56.3, 54.9 | 61.5, 60.2 | **lost** |
| RC4 children CPU / memory, run mean | **174 % / 423 MiB** | 220 / 554 | 233 / 1028 | **lost** |
| RC4 tree CPU / memory, run mean | 292 % / 754 MiB | 293 / 759 | 352 / 1332 | tied |

The four rows RC4 loses are means over the 120 s run, and go2rtc's
transcodes of the cameras that are not XM show their first picture 39 to 46 s
into it (6.5 FFmpeg on average where oxrtc has 7.6). Over the run's second
minute, when every contender's transcodes run (the samples from t = 60 s,
three repetitions): children CPU go2rtc 241, 243, 221 (235) against oxrtc
222, 217, 225 (**221**); the server's own process 162 against **78**; the tree
397 against **299**; children memory 501, 557, 515 (**524**) against 536,
552, 535 (541 MiB, 3 % behind); and per second its transcode ran camera_5
costs 0.64, 0.65, 0.61 of a core against 0.57, 0.60, 0.62, camera_11 0.59,
0.63, 0.55 against 0.59, 0.58, 0.59. The work costs the same or less; the run
mean charges oxrtc for 30 s of picture go2rtc's viewers do not have. The
bench is not changed for it and the rows stay published as lost; a row over
equal running time is W6's (BENCHMARKS.md, "Rules").

Tied behind, with equal work this time (the builder's tables had oxrtc lowest
on the webcams while x264 was told no rate, a lighter encode): the three
Motion JPEG cameras' FFmpeg costs about 5 % more than go2rtc's on the same
encoding level. go2rtc decodes and re-encodes every JPEG it relays (quality
75, 4:2:0) in its own process - 104 % of a core against oxrtc's 15 - and
hands its FFmpeg a smaller picture to decode; the tree is 275 % of a core
against 348. The children's memory is 10 MiB behind (2 %); W5 removes one
FFmpeg per camera with sound.

L03, one repetition: 2724 +/- 599 ms against go2rtc's 2485 +/- 608 (twenty
viewers; the camera's keyframes are 4.8 s apart), not worse. B03, one
repetition: first frame 715 ms against 6431, latency 130 ms against 180,
the tree 708 % of a core and 1333 MiB against 654 and 1105 - behind go2rtc on
those two as before W4, where 0.2.0 held 1710 to 1747 MiB and 723 to 752 %.

Left open, each written where it belongs (ROADMAP W4 "Not done"): FFmpeg 6.1
and 7.1 keep the guess; a fast webcam's two encoding levels; the first second
of a cold transcode of a long-GOP camera; RC4's viewers of camera_5 freezing
more since they are served 30 s sooner.

**The owner's answer, 2026-11-20: "Toujours la cadence"** (always the rate:
give x264 the camera's rate on every start). A fast webcam was encoded at 2.9
or 7.2 Mbit/s by the evenness of the half second its transcode started in
(above, "A webcam whose pictures come evenly"); he wants the ~7 Mbit/s encode
at every start, for steadier quality, and accepts that it costs more CPU than
go2rtc's on a minute where go2rtc lands on the low level. Done in W6
(2026-11-21): with no grid a Motion JPEG track is announced at its rate,
however fast (`crates/rtsp-server` `sdp.rs` `framerate`), the rate being the
mean of the newest gaps that are no pause (at most four times the mean of the
others since the W6 review, below), said as
the whole number of pictures a second within 2.5 % of it
(`crates/core` `picture_rate.rs` `rate_interval`). The median gap is no rate
of such dates: 30 pictures a second dated as they arrive up to 8 ms off their
slots have a median of 37.8 ms (26.5 a second) and a mean of 33.0 (30.3, said 30). The grid
still comes first (the ESP32 is still told 25) and a coded track keeps its
pace. Tests that failed with the rule as it was: `picture_rate_tests.rs`
`a_webcam_on_an_uneven_minute_is_read_at_the_rate_it_sends`, `sdp.rs`
`a_webcam_is_announced_at_its_rate_however_fast`, and `tests/jpeg.rs`
`a_cameras_rate_is_announced_to_ffmpeg_as_its_own_guess_read_it`, where the
real ffprobe 8.0.1 told not to guess now reads 25 and 30 of webcams on an
uneven minute (it read `0/0` before, as FFmpeg's guess does). go2rtc's level
is still FFmpeg's guess, so the bench records the bitrate each viewer received
beside each transcode's CPU (`bench/work.py`, W6): a webcam's CPU row is read
with the work done.

**The W6 review, 2026-11-21: pictures that arrive two at a time.** The pause
rule was "at most twice the median gap". A webcam whose pictures reach the
relay in pairs (a network that hands them over together) has a median gap of
almost nothing, so only the short gaps were kept: on the release build, a
local webcam of 30 a second writing its pictures two at a time was announced
`a=framerate:23226.106`, and its transcode (openIVR's own h264 template) was
encoded at `90000/1` with a fifth fewer bits than the same webcam sending
evenly (`30.000`, `30/1`). A gap is now a pause when it is more than four
times the mean of the others, and a rate above 120 pictures a second (a burst,
such as a connection's first pictures) is said as nothing; a Motion JPEG track
with no rate says nothing, not its median gap, which is the burst's too. On
the owner's two fast webcams (120 s of each one's arrivals, every window of 15
gaps replayed through both rules) the new rule is never worse: camera_5 said
its whole rate in 2635 windows of 3577 (2638 before), 14 windows off by more
than half (18), at most 48 a second (52); camera_11 2731 of 2986 (2671), 1
off (4), at most 31 (39). Both rules read 12 to 25 a second in a fresh
connection's first windows: a transcode that starts with its source is told
a rate below the camera's, which openIVR avoids by holding the source for its
recorder. Tests that failed before: `picture_rate_tests.rs`
`a_webcam_whose_pictures_arrive_two_at_a_time_is_read_at_the_rate_it_sends`,
`sdp_tests.rs` `a_webcam_is_announced_at_its_rate_however_fast` (its burst
case).

## D37 - The sound of a G.711 camera is transcoded in process; libopus is linked, through a safe wrapper (2026-11-20)

W5 (R33). Every camera with a microphone cost one FFmpeg (about 80 MiB and a
few percent of a core) to turn its G.711 into Opus, and openIVR's layout
lists two such sources per camera: its sound de-duplicator's WAV
(`ffmpeg:http://127.0.0.1:<port>/camera_N_src#audio=opus#input=dedup_input`)
and the camera's relay (`ffmpeg:rtsp://127.0.0.1:<port>/camera_N_src#audio=opus`).
That transcode is now made in process when oxrtc can read the input itself.

**What qualifies** (`crates/ffmpeg/src/native/plan.rs`, decided from the
source alone): one `#audio=` of a codec oxrtc encodes - `opus`, `pcma`,
`pcmu`, `pcm` and their `/8000`, `/16000`, `/48000` - whose template is still
go2rtc's built-in one; an input that is a stream of this server (by name, or
its loopback RTSP URL, the query's sound wish kept) or an `http(s)` URL; no
`#video=`, `#raw=`, filter, `#async`, `#query=`; and an `#input=` only on an
`http(s)` URL, when its template is made of probing flags alone
(`-fflags nobuffer`, `-flags low_delay`, `-probesize N`, `-analyzeduration
N`, one `-i {input}`: openIVR's `dedup_input`, go2rtc's `http`). Anything
else stays FFmpeg, unchanged: `opus/16000` (it says a rate and channels of
its own), an operator's own `opus` template, an RTSP URL of another server.
`ffmpeg.native_audio: false` turns it off.

**What it reads, and when FFmpeg takes over.** A stream input is read through
a subscription of the hub, the sound alone, with a recorder's queue (what
FFmpeg's read through the relay had); it is listed among the stream's
consumers as `native_audio`. An HTTP input is read with the webcams' client
(`oxrtc_http_client::Bodies`, the binary hands it over), and must answer
`audio/wav`. Its input is only known once read: a stream whose sound is AAC
or Opus, an answer that is not a WAV, a WAV of 24-bit samples or of MP3 runs
FFmpeg instead, in the same run, before anything is published
(`Attempt::NotNative`).

**What the core sees does not change.** The driver wraps FFmpeg's and answers
`declared()`, the stall and start timeouts, `local()` and `transcodes()` as
FFmpeg's did, so the selection rules, the queue classes, the idle graces and
the backoffs are those of D14, D15, D27, D34 and D36. The errors keep their
classes: an HTTP 4XX is `SourceError::Declined` (openIVR's 415 for a sound
that is not G.711, FFmpeg's `Server returned 4...`), a 5XX unreachable; a
stream that does not exist declines, as the relay's 404 made FFmpeg's run.
One differs: a stream with no sound declines, where FFmpeg, finding no sound
in the relay's description, exited unreachable; declined, a viewer's
subscription lets the route go and the source backs off slowly (D36), as for
the de-duplicator's 415 on the same camera.

**The chain** (`crates/audio`, no I/O): G.711 or L16 decoded to 16-bit
samples, mixed to one channel, resampled to the encoder's rate (a
windowed-sinc polyphase filter; libopus takes 8, 12, 16, 24 and 48 kHz
itself, so a G.711 camera's 8 or 16 kHz is not resampled for Opus), cut in
20 ms, encoded. Opus is set as go2rtc's `opus` template sets FFmpeg's
libopus: the restricted low-delay application, 64 kbit/s, complexity 10,
one channel, described `opus/48000/2`.

**The timeline (D12).** A packet is dated where its first sample was in the
input, converted to the output's clock, and `received` when that sample's
frame was received, moved by its place in the frame; the transcode's run is
anchored on the hub's timeline from that, so the sound keeps its source's
clock to the sample (`the_sound_keeps_its_sources_clock`: every packet dated
0.083 ms from its input frame, all within 1 ms of each other). FFmpeg's was
anchored at its first output, its probing and buffering later. An input frame
that does not follow the one before by more than 100 ms starts the chain
again from its own date; a WAV's samples are dated by their count, each
piece received when it arrived.

**libopus** (D28: a C dependency where no Rust implementation exists, the
gain measured). `opus` 0.4.0 (MIT or Apache-2.0, a safe wrapper, one
dependency) over `opusic-sys` 0.7.7 (BSD-3-Clause, maintained, bundles
libopus 1.6.1 and builds it with CMake). Built on Windows with MSVC and,
through `scripts/release_local.py`, for linux-amd64 and linux-arm64 with
cargo-zigbuild, CMake told the Ninja generator (else it picks Visual Studio's
and MSVC objects land in the Linux archive); its licence text is carried in
THIRD_PARTY_NOTICES by cargo-about. Not taken: `opus-head-sys` (libopus'
development head, its neural models on by default), `shiguredo_opus` (its
build script fetches sources or prebuilt libraries), `audiopus_sys`
(unmaintained since 2021), the pure-Rust ports (young, not libopus). The
fuzz targets build the crate without it (feature `opus`).

**D5 bends, and how far.** Nothing from the network is decoded in process
but G.711 and L16 (table lookups and byte order) and the WAV header (our
own reader, fuzzed: `fuzz/fuzz_targets/wav_stream.rs`, the header within
4 KiB, less than a sample frame held after it). libopus is an encoder fed
samples this process made; no decoder of a compressed format is linked.
AAC, and every other compressed input, stays in FFmpeg's confined process.

**Measured (2026-11-20, binary of 96ea696, the owner's NVR running).** The
sound's dating, by content, on his camera_1 read through his running relay
(`av_offset.py` in the W5 report: two RTSP sessions at once, the camera's
G.711 and the Opus, each put on the wall clock by its sender reports, decoded
and cross-correlated): the Opus is dated 61 and 63 ms later than the sound it
carries when FFmpeg makes it, 6 and 1 ms native - the recorder's audio/video
offset no longer moves by FFmpeg's latency. B07 (openIVR's layout, ten H.265
cameras with G.711 at 16 kHz, oxrtc alone, native against
`ffmpeg.native_audio: false` alternated, 3 runs each): 9.8 FFmpeg processes
against 19.8, the tree's memory 1600 +/- 81 MiB against 1787 +/- 47, its CPU
768 +/- 28 % of a core against 787 +/- 12 (no difference at 85 % machine
load). Against go2rtc in the same session B07 stays behind on the tree's CPU
(776 against 734) and memory (1573 against 1316 MiB), from the video
transcodes and not the sound: oxrtc's ten x264 FFmpegs hold about 150 MiB
each, go2rtc's about 100 (W6, ROADMAP).

**Corrected (2026-11-21): a packet is published at its instant, not
before.** The acceptance on the owner's cameras (RC1, six repetitions
started) showed oxrtc's viewers of his two XM cameras freezing 7 to 22
times where go2rtc's did not, the browser holding its picture 1.3 to 7.2 s
for the sound, and the sound placed "by the server's dates" -1.5 to +14 s
from its picture (once +107 s in an earlier run); with FFmpeg making the
sound (W4's acceptance) it sat within 0.16 s. Those cameras send 64 ms of
A-law a frame, which makes three or four Opus packets at once, each
`received` when its first sample was (the frame's arrival moved forward by
that sample's place in it, up to 44 ms); all were published at once, so
two in three reached the WebRTC sender before their instant, and str0m's
sender report written after a frame whose wall clock is in the future
carries RTP time 0 (`current_rtp_time`, as `session_sound.rs` already
says): the browser was handed a broken mapping for a large share of the
sound's reports. FFmpeg's packets are received when they arrive, never
ahead. The publisher now keeps its packets in order and publishes each at
its instant (`publish.rs` `publish_due`, the stream and WAV runs waking for
the next one); dates and instants are what they were, so the sound keeps
its source's clock as above, and the browser, which plays by date, hears
nothing later. Pinned by `no_packet_is_received_in_the_future` (a clip of
the XM cameras' shape: FAILED before, a packet read 43 ms before its
instant). Found with `w5-work/dating_probe.py` of the W5 report (each
track of openIVR's layout, read on a relay of its own fed by the owner's,
put on the wall clock by its sender reports) and a trace of the first
frame each session anchored on. A second fact it showed is R49, below.

Not done here: G.722 is in `oxrtc-audio` (bit for bit FFmpeg's encoder) but
no reader can ask for it yet - the core has no G.722 codec and go2rtc no
`g722` template; it comes with the codec (M8). A camera's AAC is FFmpeg's.
Measured, not W5's (2026-11-21): the RTSP server dates each track of a
session from that track's own first frame (`crates/rtsp-server/src/packetize.rs`,
`first`). A picture track starts at the cached keyframe, a sound track at
the packet of the moment, and a camera's start arrives as a burst: on the
owner's camera_1 read through his relay, a session of the viewer's stream
opened 75 ms after the start anchored its sound 5.9 s from its picture.
Sessions of `camera_1_rec`, the recorder's stream, opened 0.1 to 14 s after
a start with openIVR's hold on it, date the sound -1972 to +685 ms from its
picture with native audio and -3524 to +705 ms with FFmpeg's: the rule, not
the transcode. R49 (a fix changes how every RTSP reader is dated).

**Accepted (2026-11-21, binary `aebc962`, six repetitions, abba, the
owner's NVR running; the W5 report's `w5r6-*` sessions).** RC1, against
go2rtc 1.9.15 and MediaMTX 1.21.3, on his four reachable cameras: the FFmpeg
children's memory won (378.2 +/- 7.9 MiB against 420.0 +/- 8.2 and
933.0 +/- 11.3), the tree's CPU (252.5 against 341.5 and 300.6 % of a core)
and memory (418 against 484 and 1006 MiB) won; the children's CPU tied
(236.9 +/- 9.5 against go2rtc's 233.0 +/- 6.4): the two sound FFmpegs cost
1 to 2 % of a core each, and what remains is the four x264 transcodes,
where oxrtc's webcam transcodes cost 1.4 and 5.0 points more than go2rtc's
(W4, W6). Native against `ffmpeg.native_audio: false`, oxrtc alone,
alternated: RC1 374 against 420 MiB of children and two processes fewer at
no CPU difference; B07 1578 +/- 41 against 1791 +/- 19 MiB of tree and ten
processes fewer, no difference in CPU, latency or freezes. B07 against
go2rtc stays behind on the tree's CPU (762 against 723) and memory (1584
against 1316 MiB), from the video transcodes as above.

**Reviewed (2026-11-21).** A WAV input was read as fast as its server sent
it: a server writing an endless A-law stream at the network's speed had
9000 s of sound read, transcoded and published in 5 s, on a runtime thread
(FFmpeg would have read it as fast, in a process of its own). The WAV run now
reads no further ahead of the wall clock, from its first sample, than the
lead the core's timeline allows a source (`MAX_LEAD_SECS`, 10 s): TCP holds
the rest at the server. openIVR's de-duplicator, which replays about 4 s of
a camera's past at once and then follows the camera, is never held. Pinned
by `a_wav_served_faster_than_time_is_read_at_most_the_lead_ahead` (FAILED
before: 449995 packets of 20 ms in 5 s; now 750, the lead and the time
elapsed). And an HTTP input that is not a WAV (an FLV or MPEG-TS camera with
AAC) was asked again on every run: each attempt opened the server twice, the
native `GET` and then FFmpeg's, back to back - a camera that rations its
connections may refuse the second each time. Found once not a WAV, an HTTP
input is FFmpeg's from the start of every later run of that source; a
stream input is still asked each run (a subscription, no connection, and its
camera may change codec). Pinned by
`an_http_input_found_not_wav_is_not_asked_again` (FAILED before: three
native `GET`s for FFmpeg's three; now one). And a packet's instant could go
back: one that starts in the samples the frame before left over is placed
back from its own frame's arrival, and when that frame came late (frames
delivered in pairs, or the source's timer catching up) that was before the
last packet of the frame before - `no_packet_is_received_in_the_future`
failed so in 2 of 40 runs on a loaded machine. A packet is never placed
before the one before it; `no_packet_goes_back_when_frames_come_in_pairs`
(the XM clip two frames every 128 ms) FAILED before, 16 instants back in 100
packets, and the first test passes 40 runs of 40 under the same load.

## D38 - Fragmented MP4: decode times contiguous per track, the PCM family as FLAC of verbatim subframes (2026-11-21)

M5, F1: `crates/mp4` (`oxrtc-mp4`), a library with no I/O that the MSE, MP4,
HLS, DASH and recording items write with. Five choices that the egresses
built on it inherit.

**Decode times are contiguous per track** (`crates/mp4/src/timing.rs`). A
sample starts where the previous sample of its track ended; its duration
reaches where the next one is expected (its own decode time plus the track's
typical step) and, when the next frame comes while the sample is still
unwritten (a fragment of several frames), exactly to it. Why: MSE's coded
frame processing takes a decode time more than twice the last frame's
duration after it for a discontinuity and drops every frame until the next
keyframe, and a gap in what is buffered stops playback; a camera's clock is
uneven and loses frames. One fragment per frame cannot know a picture's
duration when it is written. go2rtc dates each frame at the previous frame's
time with the step just measured (contiguous, one frame early); oxrtc places
it at its predicted time, so a steady source is placed on its own times
exactly (ffprobe reads back the source's decode times on every FFmpeg case)
and an uneven one within one step's misprediction, never accumulated. The
typical step is the sound frame's own length for audio and, for video, the
shortest of the last eight steps (1/25 s before there are two), held to
1 tick to 1 s: a lost frame makes one long step, which taken as the
estimate made the next frames expected late and squeezed each to a tick,
never shown; a frame rate that drops is followed eight frames later. Proven
in headless Edge: a picture with steps of 32 to 48 ms and 200 ms missing at
3 s, fed one fragment per frame at its own time, played 5.9 s in 5.9 s with
one buffered range and no stall.

**Decode times are the core's timeline** (D12), not rebased to zero: both
tracks of a writer start where the hub placed their first frames, so they
stay in step, and HLS can derive `PROGRAM-DATE-TIME` from a fragment's
`Span`. An MSE player seeks to `buffered.start(0)` (the browser proof does);
go2rtc's player does not, and the MSE egress moves each socket's times to
zero before they reach the writer (D40).

**The PCM family is FLAC** (`crates/mp4/src/flac.rs`): MSE takes neither
G.711 nor raw PCM in MP4, FLAC it does, and go2rtc gives browsers the same.
A-law and mu-law are decoded with `oxrtc-audio`'s decoders (no second copy),
L16 as it is; each source frame is one FLAC frame of a variable-block-size
stream (its first sample's number in the header), with verbatim subframes:
nothing to compress, no dependency, and a decoder gives back the very
samples - FFmpeg's FLAC decoder on the written track gives byte for byte
what its own A-law decoder gives on the input. Stereo is two independent
subframes (go2rtc halves the channels into a doubled rate).

**Fragmenting is the caller's** (`Fragmenting`): one fragment per frame, per
group of pictures (a second of sound with no video), or per duration with a
cut before every keyframe of the first video track, so that a keyframe
always starts a fragment; each `Fragment` says whether it is independent
(every video track's first sample in it is a keyframe), where it starts and
how long it lasts. A fragment is also closed before 4096 samples or 32 MiB.

**H.265 is `hvc1` by default** (what Safari plays), `hev1` by option. The
core's frames carry the parameter sets in band ahead of every keyframe
(`oxrtc-depacketize`'s `EachKeyFrame`), so `hev1`'s three arrays are marked
incomplete, as they are; `hvc1` requires them complete (ISO/IEC 14496-15,
8.4.1) and is written so, the in-band copies left in the samples, as go2rtc
does - headless Edge plays both, the FFmpeg cases are fed such frames. A composition offset is
written (a version 1 `trun`, signed) only when a frame's decode time differs
from its presentation time, which the core's frames never say: the caller
that knows one (`push_decoded`) gives it.

Changes if: a browser is found to want a gap rather than a corrected
duration (none of Edge's cases did), or the core's frames gain a decode time.

## D39 - go2rtc's WebSocket API: WHEP's session over a socket, outliving it; the handshake authenticated as every route, no credential in the query (2026-11-21)

M5, F2 (its WebRTC part): `GET /api/ws?src=<stream>` (`crates/api/src/ws/`),
the signalling of Home Assistant's go2rtc integration, the WebRTC Camera
component, Frigate's cards and go2rtc's own player. What go2rtc 1.9.15 does
was measured on free ports with its own binary (an `exec:` FFmpeg loop of an
H.264 and Opus clip as the source, aiortc as the client), and oxrtc does the
same except where said.

**One session, WHEP's.** An offer goes through the API's `WebRtcAnswerer`,
the call `POST /api/webrtc` makes: the same session, its resource (D23),
its total and per-address bounds (D9), one MediaStream and lip sync (W1,
W1b, W1c). Both of go2rtc's dialects are read: `webrtc/offer` with the SDP
as its value, answered `webrtc/answer` (go2rtc's player), and `webrtc` with
`{"type":"offer","sdp":...}`, answered `webrtc` with `{"type":"answer",
"sdp":...}` (Home Assistant's library writes only this one). The offer's
`ice_servers` (Home Assistant sends its own) are not used: a server that
gathers through STUN or TURN servers its client names sends to addresses of
the client's choosing; the server's own (`webrtc.ice_servers`, `PUT
/api/webrtc/ice`) are used. A candidate the client sends goes to the
session as a `PATCH` body would (`a=<line>`, its first line only: what
follows a line break would be lines of the client's choosing in the
fragment), under the same bounds; one sent before the answer is held (64 at
most) and handed over once the session exists; an empty one asks nothing.

**The answer carries every candidate, and each is sent as a message too.**
go2rtc answers before it gathers (no `a=candidate` in its answer), then sends
its candidates as `webrtc/candidate` messages, all within a millisecond of
the answer (measured: four). oxrtc's answer is complete, as its WHEP answer
is, and each of its `a=candidate` lines then follows as a `webrtc/candidate`
message: a client that reads only the answer and one that reads only the
messages both connect (aiortc does either).

**The session outlives its socket.** go2rtc ties nothing to the socket's
close (its `internal/webrtc` registers no close handler): measured, a
session whose socket closed 2 s after the answer kept sending for the 40 s
watched, until the client closed its peer connection. Its player depends on
it: once WebRTC plays it closes the socket itself (`onpcvideo`). oxrtc does
the same; the session ends as any session does (its peer gone, its idle
timeout, `DELETE` of its resource), and its resource with it. One exception,
which no client can see: a socket that closes before its offer is answered
leaves a session nobody can use, which go2rtc listed as a consumer for 30 s
(measured); oxrtc ends it as soon as it exists.

**go2rtc's error form, in one place for the later items.** A failure is
`{"type":"error","value":"<type>: <reason>"}`, the reason redacted (a
backend's message may quote a source's URL): `webrtc/offer: stream not
found` (go2rtc's very words) for an unknown or absent `src`, `no matching
codec`, `streams: <reason>` for a source that cannot open, `too many
sessions`, `bad offer: <reason>`. A type go2rtc has no handler for is read
and ignored, as there. `mse`, `hls`, `mjpeg` and `mp4` are answered `<type>:
not supported by this server yet` by one function (`conn.rs`, `later_error`)
that F3 to F6 replace: go2rtc's player reads an error about `mse` as "try
MJPEG" and one about `mjpeg` as nothing more to try, so a player that asks
for every mode still plays WebRTC. Text that is not a message closes the
socket (`1007`; go2rtc closes it with no close frame), and so does a binary
frame (`1003`).

**The handshake is authenticated as every route**: the router's middleware
reads `Authorization` (Basic or Bearer) on the upgrade request, loopback
included (D7). Who sends what:

- Home Assistant's library (`go2rtc-client` 0.4.0) opens the socket with
  aiohttp's `ws_connect` on the session it was given: a session with a
  `BasicAuth`, or a server URL with `user:password@`, both send
  `Authorization: Basic` on the handshake (measured, both connected).
- A browser cannot set a header on a WebSocket. It sends the Basic
  credential it holds for the server's origin, which it holds once a page of
  that origin was challenged and given it - the server's own UI, or a page
  served through the same origin (measured in headless Edge 157: see
  F2's report, section 5).

**A credential in the query string is not accepted** (`?token=`, `?auth=`):
a URL is written by every reverse proxy's access log, kept in a browser's
history and sent in `Referer`; oxrtc's own log never writes a request's query
(only its path, and a WHEP resource's id elided), but a proxy's does. Nothing
needs one today: a same-origin page has its Basic credential, a program sets
the header. A page of another origin that must open a socket is F20's
(MediaMTX takes a JWT in the query for that, and that design is decided
there, with its expiry).

**The page's origin is checked as go2rtc checks it** (`api.origin`, read
from the same key): a handshake with no `Origin` (not a browser's) is taken;
one whose `Origin` is the request's own `Host` is taken; when that `Host`
names no port (a reverse proxy passing its own host name on, its port the
scheme's), a page of that host on any port is taken too; `api.origin: "*"`
takes any page; any other value, only a page of exactly the request's
`Host`, as go2rtc leaves the check to its WebSocket library then. Anything
else is refused `403` before the upgrade. This is what stops a page of
another site from opening a socket with the credential a browser holds for
the server. That is go2rtc 1.9.15's default as measured with a handshake's
headers (F2 review): `Host: 127.0.0.1:43384` with `Origin:
http://127.0.0.1:9999` is `403`, `Host: 127.0.0.1` with the same `Origin` is
`101`. The first version of this decision took a page of the request's host
on any port whatever the `Host` said, which go2rtc does not: a page another
program serves on another port of the server's machine is another origin,
and could open the socket with the browser's credential for the server.

**Bounds** (`WsLimits`, D9's rule): 128 sockets at once, half the API
listener's 256 connections, so that sockets never take every slot plain
requests need, and 64 per address (twice a caller's WebRTC sessions: Home
Assistant holds one socket per viewer of its dashboards, all from its one
address), refused `503` beyond; a message (and a frame) of 64 KiB at most,
as a request body; 50 messages a second, pings and pongs counted (each is a
frame read and a pong written), one more closes the socket (`1008`); a
candidate line of 1 KiB at most (a browser's longest is under 200 bytes), so
that the 64 held before an answer are 64 KiB a socket and not 64 messages'
4 MiB (F2 review); one offer answered at a time and 4 sessions in a
socket's life; a ping every 20 s, and a socket from which nothing came for
60 s (no pong) is closed (go2rtc pings nothing and closes nothing: measured, a silent socket stayed
open 130 s); a message that cannot be written within 5 s (go2rtc's write
deadline) ends the socket, and the writer buffers 256 KiB at most. The
server's stop closes every socket with `1001`: a socket is not a request,
and the listener's graceful stop does not wait for it.

**The listener's request-head clock does not run on frames.** The API's
listener cuts a connection whose request head takes longer than 10 s from
its first byte (`HEAD_TIMEOUT`); after an upgrade every frame looked like the
first bytes of a request head that never completed, and a socket was cut 10 s
after its first message (`the_request_head_clock_does_not_run_on_frames`
failed before: the socket closed). The upgrade marks the connection, whose
reads then start no head clock; its idle timeout (75 s) still runs, and the
socket's pings keep it from firing.

Changes if: a client is found that cannot take an answer carrying
candidates or the repeated messages, a client that needs the session to end
with its socket (none of go2rtc's), or F20 decides a token for pages of
another origin.

**Confirmed by the owner, 2026-11-23** (M5 item D (2), his answer to the F2 review's note): the
strict rule stays, as go2rtc's. A page of another origin than the request's `Host` is refused, a
reverse proxy passes the browser's `Host` on (or `api.origin: "*"` is set), and a program with no
`Origin` is always taken. What changes is what the refusal says: go2rtc 1.9.15 answers `403
Forbidden` (10 bytes) and logs `websocket: request origin not allowed by Upgrader.CheckOrigin` with
the host and the origin at `ERR` for every refusal (measured: `target/d-work/o/run.sh` of the M5
lane's report D.md); oxrtc answers `403` with the page's origin, the `Host` it reached, and the two
ways out (`a reverse proxy passes the browser's Host header on (nginx: proxy_set_header Host
$http_host; Caddy does by default), or api.origin: "*" lets every page in`), and logs the same at
`warn` once per interval (a page retrying in a loop is one line). Both values are the requester's
own headers, printable ASCII, 128 characters each at most. COMPATIBILITY.md carries the nginx and
Caddy recipes. openIVR does not reach `/api/ws` (its backend forwards WHEP with no `Origin`), so the
rule cannot refuse it today.

## D40 - MSE over go2rtc's WebSocket API: the browser's list picks the sources as a WebRTC offer does; each socket's media starts at zero; the subscription's queue is the socket's (2026-11-21)

M5 F3. `{"type":"mse","value":"<codecs>"}` on `/api/ws` (`crates/api/src/ws/mse.rs`,
`muxer.rs`) answers `{"type":"mse","value":"video/mp4; codecs=\"...\""}`, then binary frames:
the init segment of `oxrtc-mp4` (F1, D38), then one fragment per frame from a keyframe. go2rtc's
player sends it from its `MediaSource`'s `sourceopen`, its fixed list (`avc1.640029`,
`avc1.64002A`, `avc1.640033`, `hvc1.1.6.L153.B0`, `mp4a.40.2`, `mp4a.40.5`, `flac`, `opus`)
filtered by `MediaSource.isTypeSupported`, and makes one `SourceBuffer` of the answer.

**The list makes a `Want`, and the core picks the sources** with the same rule as a WebRTC
offer (`crates/webrtc/src/offer.rs`): the first source of the stream that offers an accepted
codec, in the one order every egress shares (`oxrtc_core::PREFERENCE`, `Want::preferred`, which
WebRTC's offer reads too since the F3 review): video H.265 then H.264, audio Opus, AAC, then the
PCM family. So on
openIVR's `camera_N` (the camera relayed, then its H.264 transcode) a browser that lists `hvc1`
or `hev1` gets the camera's own H.265 and no FFmpeg is started for it, and one that does not
gets the transcode - measured in headless Edge 157 (zero FFmpeg process) and by
`apps/oxrtc/tests/ws_mse.rs`. A name is read by its family whatever its profile and level
(`avc1`/`avc3`, `hvc1`/`hev1`, `mp4a.40`, `opus`, `flac`), because the player lists fixed
strings, not the stream's: go2rtc matches the exact strings. `flac` takes the PCM family
(G.711 A-law and mu-law, L16), which the writer packs as FLAC. No sound in the list: the picture
alone. `hev1` alone in the list gives the `hev1` entry; else `hvc1`, Safari's.

Where go2rtc differs, on purpose:

- **The answer names the stream's real codecs** (`avc1.42c01f`, `hvc1.1.6.L153.90`, from the
  parameter sets), where go2rtc writes fixed strings for H.265 and AAC (`hvc1.1.6.L153.B0`,
  `mp4a.40.2`, an HE-AAC stream included): the `SourceBuffer` is made for what comes.
- **A list with nothing this server sends is `mse: codecs not matched`**, the socket kept: go2rtc
  gives it its default (H.264 or H.265 and AAC), which that browser cannot play. An empty list
  (or none) gets go2rtc's default.
- **A stream whose tracks come back with other codecs** (a source replaced by another) ends the
  socket with an error: one `SourceBuffer` cannot change its type in go2rtc's player, which
  starts again when its socket closes. Other parameter sets with the same codec strings get a
  new init segment in the same `SourceBuffer`; another profile or level changes the string the
  `SourceBuffer` was made with, and ends the socket as other codecs do (F3 review: what the
  muxer compares is the MIME type).

**Each socket's media starts at zero.** Times are the core's timeline (D12) moved back by
one constant, the time of the first frame the socket writes, in each track's clock, so the
tracks stay in step (amends D38's last word on MSE: go2rtc's player does not seek to the first
buffered time). Measured with go2rtc's player in headless Edge 157 on the core's timeline
untouched: the player plays from `currentTime` 0, seeks only once 5 s are buffered, then sets
its `playbackRate` to the distance to the live edge - it sat 5 s behind, played at five times
the speed, its `remove()` of what is older than 5 s took the pictures it was showing up to the
next keyframe, and H.264 720p with sound decoded 8 to 20 pictures in 13 s (H.265 at a low
bitrate kept up). Moved to zero: 4.0 s played in 4.0 s in all four cases, 25 pictures a second,
1 s behind the live edge, as go2rtc 1.9.15 on the same streams and the same page.

**The first fragment is the cached group of pictures' keyframe**: the subscription starts from
the core's cache (`api.gop_cache_mib`) as a WebRTC session's does, and the muxer writes nothing,
sound included, until the video's keyframe. A viewer joining a running stream had its picture
in 101 ms in headless Edge (the cached pictures sent at once - this camera's groups last 2 s, 2.7
MB - nothing dropped, 25 pictures a second after), where go2rtc waits for the camera's next
keyframe.

**The subscription's queue is the socket's.** The socket writes each fragment before it reads
the next event, and its writer holds one message (every send waits for its flush), so a reader
slower than the stream backs up the core's queue for that consumer - the live budget, 2 s and
8 MiB - which drops what it cannot hold and resumes at a keyframe (`oxrtc_core::queue`); the
muxer then writes nothing until the video's next keyframe, the stream's listing counts the frames
(`dropped`), and the socket logs `mse reader fell behind the stream` at info, once per 10 s. No
buffer of the socket's own grows, and no other reader waits: the producer enforces the budget
when it publishes. A reader that takes no data for 5 s ends its socket (the write deadline).
The writer's bound is raised to one fragment (twice the core's `max_frame_bytes` and 64 KiB: a
G.711 frame as FLAC is twice its size) - at 256 KiB a camera's keyframe of 1 MiB failed the
write and ended the socket (`a_fragment_larger_than_a_message_is_written_whole` failed before).

**One MSE per socket at a time**, counted with the offers against the 4 sessions a socket may
ask for in its life; a second while one runs is `mse: already streaming on this socket`. A
failure before the MIME type is sent (`stream not found`, `codecs not matched`, `streams: ...`)
is an error the player answers by trying its next mode; one after it ends the socket. The
socket's end ends the subscription at once.

Changes if: a player is found that sets `timestampOffset` itself or needs the core's times (HLS
keeps them: D38), a browser that needs go2rtc's fixed strings, or a reader for whom 2 s of queue
is too short (a `queue=` for MSE, as RTSP has).

## D41 - A camera's H.265 goes to a browser at a profile, tier and level its offer takes, else the stream's H.264; the answer names the camera's own (2026-11-22)

M4, W7. openIVR's `camera_N` lists the camera's own picture first and its
H.264 transcode second; a browser whose offer takes H.265 is to get the
camera's picture with no transcoder started.

**What the browsers offer** (the owner's PC, RTX 4080, 2026-11-22,
`RTCRtpReceiver.getCapabilities('video')` and a recvonly offer): Chrome 157,
headed and headless, with no flag, offers
`level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST` and the same with
`profile-id=2` (Main and Main 10, Main tier, level 6.0). Edge 157 offers no
H.265 for WebRTC, headed or headless; forced to with
`--enable-features=WebRtcAllowH265Receive` it offers the same lines and its
page crashed during the session (Playwright's `Target crashed`, with the
720p libx265 camera and the owner's XM camera alike, headed and headless). MSE's `isTypeSupported('video/mp4; codecs="hvc1..."')` is true in
both (why F3's MSE got H.265 in headless Edge). The observation of
2026-11-17 that every answer carried H.264 only was the browser's side: the
bench's headless Edge offers no H.265.

**What oxrtc did** (0.2.0 at eb7aef1): any H.265 offer got the camera's
picture and an answer of Main, level 6.0 (`enable_h265`), whatever the
camera: a Main 10 camera was answered `profile-id=1`, a camera above the
browser's level was sent anyway. On the 720p libx265 camera it already
played with no FFmpeg (301 pictures in 12 s in headless Chrome).

**The rule now.** RFC 7798 7.2.2: `profile-id` and `tier-flag` are used
symmetrically, and a receiver's `level-id` is the highest level it decodes.
The camera's SPS gives its general profile, tier and level
(`crates/core/src/h265.rs`); when one of the offer's H.265 payload types has
its profile and tier at a level at least the camera's, the camera's picture
is sent and str0m is told the camera's numbers (`add_h265`), so the answer is
on that payload type and names the camera's level (`a=fmtp:49
profile-id=1;tier-flag=0;level-id=150` for the XM cameras). Otherwise the
offer without its H.265 is subscribed to, which picks the stream's next
source (openIVR's transcode), the camera's subscription held until that one
has tracks. A stream with no other source sends the camera's H.265 anyway,
as before: a picture the browser may decode beats none; str0m is then told
the numbers of the offer's first H.265 payload type, so that it matches one
(told the camera's, a Main 10 camera found no payload type in a Main-only
offer and the answer carried no video; W7 review). A camera whose SPS
is not known yet, or of a profile space other than 0, is taken on trust.

**What the references do** (the same offer from headless Chrome 157):
go2rtc 1.9.15 answers every H.264 and H.265 payload type it has, sends the
H.265 and starts the transcode beside it (one FFmpeg for a viewer that never
gets its output; 9.7 s to answer); MediaMTX 1.21.3 has no transcoder,
copies the offer's `fmtp` and checks no level. On a 720p camera labelled
level 6.2 (libx265 `level-idc=62:high-tier=0`) both send it and Chrome
decodes it (312 and 273 pictures), where oxrtc sends the transcode (315
pictures, one FFmpeg). That is the price of the RFC's reading: a decoder
that decodes above its level for a small picture is not something the
answer can know.

**Measured on the fixed build** (80a3ca0, release, w7-work/ of the W7
report): headless Chrome on the libx265 camera (Main, 5.1): answer
`level-id=153`, 301 pictures of 1280x720 in 12 s, none dropped, no FFmpeg;
on the owner's camera_1 read through his relay (Main, 5.0, 2304x1296):
`level-id=150`, 233 pictures in 15 s, no FFmpeg; on the level 6.2 camera
(Main tier, then High tier): the transcode, `the viewer does not take this
camera's H.265; sent the stream's H.264` at info, one FFmpeg. Headless and
headed Edge on camera_1: the transcode, as before.

Changes if: a browser is found that decodes above the `level-id` it offers
and the transcode it then gets costs a row (send by profile and tier alone,
the level only logged); Edge offers H.265; MSE takes the same rule (F3's
review left its codec list without the profile and level check).

**Amended 2026-11-23** (M5 item D (4), the owner's answer to W7's question): an explicit level the
browser declares stays a ceiling, as RFC 7798 says (the browser said what its decoder takes), but
an absent `level-id` is no longer read as level 3.1 (RFC 7798 7.1's default): it is read as no
ceiling, as go2rtc 1.9.15 and MediaMTX 1.21.3 behave for every offer (W7 measured both sending a
camera labelled level 6.2 to Chrome's level 6.0, which decodes it). Read as 3.1, every real
camera - the owner's XM cameras are 5.0 - would go to the transcode for a browser that writes no
`level-id` (Safari 18 may, not measured: no Mac), the worst outcome for the reason a level is
given at all. The same for an MSE string that stops before its level. str0m 0.25 keeps the three
numbers only when all three are given (`sdp/data.rs` `parse_pairs`), and drops `profile-id` too
when they are not: for such a partial `fmtp` oxrtc reads the payload type's own `level-id` from
the offer's text (`offer.rs` `written_level_id`, fuzzed with the offer by `whep_offer`), so a
written level stays a ceiling and an absent one is none; its profile and tier stay str0m's
reading (Main, Main tier), since str0m answers that payload type so and a camera "taken" on the
line's own `profile-id=2` would get an answer with no video. What is left for str0m: a partial
`fmtp` of Main 10 or of the High tier is read as Main of the Main tier (ROADMAP D (4)). A Main
camera to an offer of Main 10 alone is now answered on the Main 10 type (its compatibility flag,
D44) where W7 sent the transcode.

## D42 - MP4 over HTTP: stream.mp4 and frame.mp4 written by MSE's feed; duration counted on the media; one init segment per file; their own bounds (2026-11-22)

M5 F4. `GET /api/stream.mp4` and `GET /api/frame.mp4`, as go2rtc 1.9.15 serves them
(`internal/mp4/mp4.go`), are written by the one fragmented-MP4 path there is: F3's
subscription and muxer, moved out of `ws/` to `crates/api/src/fmp4.rs` and `fmp4/muxer.rs`
(`Feed`, shared by MSE and HTTP), over F1's `Writer`. The same init segment, one fragment per
frame from a keyframe, the media moved to start at zero with the tracks in step, the
subscription's live queue as the reader's (a slow reader loses frames, resumes at a keyframe,
`dropped` in the listing, `mp4 reader fell behind the stream` in the log); a reader that names
`queue=recorder` reads with the recorder's budget, as an RTSP reader does.

**The query, as go2rtc reads it** (`mp4.rs` `stream_wish`): `mp4` (its first value) chooses
among go2rtc's three sets - `mp4` (or empty) H.264 or H.265 and AAC, `mp4=flac` and the PCM
family (as FLAC), any other value (`mp4=all`) and Opus; without `mp4`, go2rtc's `video` and
`audio` filters, narrowed to what the writer takes; with neither, go2rtc's default, H.264 or
H.265 and AAC. The sources are chosen in the order every egress shares (`PREFERENCE`: a camera's
own H.265 before an H.264 transcode), where go2rtc's lists H.264 first: on one source that has
both they differ, on openIVR's `camera_N` (the camera, then its transcode) both take the camera.
`frame.mp4` takes the picture alone, H.264 or H.265. `hvc1` always (go2rtc names `hvc1` in its
`Content-Type` and writes `hev1`).

**`duration` is counted on the media**: the response ends before the first frame at or past
`duration` seconds from its zero, so `duration=5` holds five seconds of pictures (ffprobe: 125
at 25 a second, the last at 4.96 s) even when the cached group of pictures comes first in a
burst; and, for a stream whose media runs slower than the clock, `duration` and one second
(`LATE_GRACE`, a live picture's late arrival) after its first fragment on the wall clock.
go2rtc counts the wall clock from the head, and its file of `duration=5` held 3 to 4 s (it
waits for the camera's next keyframe within those 5 s). Whole seconds; 0 is none (go2rtc's);
a value that is not a whole number is `400 invalid duration`, where go2rtc ignores it and
streams for ever.

**The first fragment is the cached keyframe**, as for every egress: a file starts at once
(`frame.mp4` answered in 11 ms on a running source, go2rtc 1.9 s), and the file of `duration`
covers the last group of pictures before the request. The price, for a player of the
progressive file that plays from its start (`<video src>`, VLC, ffplay), is that it runs up to
one group of pictures behind the live edge for as long as it plays (2.4 s with 2 s groups in
headless Edge); go2rtc's starts up to a group later and at the edge. `api.gop_cache_mib: 0`
gives go2rtc's behaviour for every egress.

**One init segment per file.** The head is held until the first fragment (F4 review): tracks
that come again before it (a camera whose parameter sets in band are not its SDP's, which the
RTSP client publishes again at its first keyframe) replace the init segment and the MIME type,
so a cold source's file is headed by the sets its pictures use. After it, a stream whose tracks
come back otherwise (other parameter sets, another codec) ends the response there, a complete file: a progressive MP4 has one
`moov`, and FFmpeg's demuxer skips a second one as a duplicate (its source), so the pictures
after it would be decoded with the first one's parameter sets. go2rtc keeps its first init
segment whatever comes.

**`filename`** gives `Content-Disposition: attachment; filename="..."`, go2rtc's form, the name
kept to 128 characters with what cannot stand in quotes (a quote, a backslash, a slash, a
control character, anything not ASCII) replaced by `_`, and RFC 6266's `filename*` beside it for
a name that is not ASCII. go2rtc writes the value as given (a quote ends it early).

**Bounds of their own** (`Mp4Limits`, `crates/api/src/slots.rs`, the counter `/api/ws`'s
sockets now use too): 64 responses at once (a quarter of the listener's connections) and 16 per
address, `frame.mp4` included (`503 too many mp4 responses`, the refusals logged once per
interval); the head (subscribing, the tracks, the init segment and the first fragment) under the slow
routes' 60 s;
a `stream.mp4` that writes nothing for 75 s (the listener's idle rule) ends; a `frame.mp4` waits
30 s at most for its keyframe (past the 28 s a subscription waits for a source still starting,
D27; `500 no keyframe within 30s`); a client gone (its write failing) or the server's stop ends
the body, and the subscription with it, and the server's stop a head still waiting (F4 review). Authentication as every route.

Where go2rtc differs and oxrtc does not follow it: a Safari `User-Agent` without `duration` is
redirected by go2rtc to `stream.m3u8` (`301`); oxrtc serves the MP4 until it has HLS (F6). A
Chrome `User-Agent` with no `Range` gets from go2rtc an empty `200` for `frame.mp4` (a
workaround for Chrome 105's two requests); oxrtc answers the file to both requests. go2rtc's
`rotate` and `scale` (patches of the init segment's matrix and `pasp`) are not done. A `src`
that names no stream is `404`, as for every route (go2rtc may create one from a URL there).
`src` absent is `400 src is required` (go2rtc: `404`).

Changes if: a player of the progressive file is found that needs go2rtc's start at the next
keyframe (a `?` to choose it, per request), a player that takes a second init segment, or a
client of `duration` that wants go2rtc's wall clock.

**Amended 2026-11-23** (M5 item D (5), the owner's answer to the F4 question): the immediate start
stays the default, and a request may ask for go2rtc's start instead: **`start=next`** waits for the
first keyframe that comes after the request, then follows live; `start=cache` (or no `start`) is
the default, the cached group's keyframe at once. go2rtc 1.9.15 has no such parameter: its
consumer drops everything until a keyframe (`pkg/mp4/consumer.go`, `start`), always, since it
keeps no group for a new reader; the name is oxrtc's (`start`, go2rtc ignores it, so a URL that
carries it works on both) and its values say where the media starts. The same `start` on
`/api/ws?src=...&start=next` for an `mse` of that socket (go2rtc's player cannot ask it: it opens
`api/ws?src=` alone, but a page or a card of its own can), and on `frame.mp4`, where it means a
picture taken after the request rather than the cached keyframe (up to a group old). Any other
value, or `cache` and `next` together, is `400 invalid start (cache or next)`. It is a core
setting of the consumer (`ConsumerTag::start`, `Start::NextKeyframe`): its first routes attach
without the cached group, and a later change of source resumes from the new source's cache as
for any reader.

## D43 - An offer whose video lists no codec this server sends is refused when the stream has a picture, in go2rtc's words with MediaMTX's status; answered with the sound when the stream has none (2026-11-23)

M5 item D (1), the owner's answer of 2026-11-23 to the F2 note. A WebRTC offer whose receiving
video line lists only codecs oxrtc does not send (VP8, VP9 or AV1 alone: an old browser, a
VP8-only client, aiortc told so) was answered with its sound alone, the video line rejected
(port 0), and aiortc 1.16 refused that answer ("Failed to set remote video description send
parameters"): the client got nothing, and no message said why.

**Measured** (aiortc 1.16 as the client, a stream of H.264 and Opus and one of Opus alone,
`target/d-work/vp8probe.py` of the M5 lane's report D.md), go2rtc 1.9.15 and MediaMTX 1.21.3:

| Offer, stream | go2rtc 1.9.15 | MediaMTX 1.21.3 | oxrtc before | oxrtc now |
|---|---|---|---|---|
| VP8 + Opus, H.264 + Opus | `500 unable to populate media section, RTPSender created with no codecs`; `/api/ws` the same text | `400 {"status":"error","error":"codecs not supported by client"}` | `201`, video rejected; aiortc refused the answer | `400 streams: codecs not matched: video:H264, audio:OPUS => video:VP8, audio:OPUS`; `/api/ws` `webrtc/offer: ` and the same |
| VP8 alone, H.264 + Opus | `500 streams: codecs not matched: video:H264, audio:OPUS => video:VP8` | the same `400` | `400 no matching codec` | `400 streams: codecs not matched: ... => video:VP8` |
| VP8 + Opus, Opus alone | `500 unable to populate media section ...` | `201`, the video line answered `VP8`, sound played | `201`, video rejected; refused by aiortc | `201`, the video line answered `VP8` (nothing sent on it), sound played |
| H.264 + Opus, either | `201` | `201` (Opus alone: video `port 0`, refused by aiortc) | `201` | `201`, unchanged |

**The rule, in one place** (`WebRtcServer::answer_resource`, which `POST /api/webrtc` and
`/api/ws` both call): an offer with a receiving video line on which it lists no codec this
server sends asks the stream for its picture in any codec (`OfferedCodecs::answer_want`); if a
picture comes, the offer is refused, before any session is made; if the stream has none, the
sound is answered and the video line is answered on the offer's own codec, on which nothing is
sent (`negotiate::rtc_config`), as MediaMTX does, so that a client that refuses a rejected line
(aiortc) still hears the sound. An offer that lists nothing this server sends is refused at
once, its stream side read from what the stream's sources showed (no source is started for a
refusal).

**Status `400`, as MediaMTX**: the offer is what cannot be served, a client's error; go2rtc's
`500` says the server failed, and go2rtc keeps such a refused consumer listed about 30 s (F2's
measure). **The words are go2rtc's matcher's** (`streams: codecs not matched: <the stream's> =>
<the offer's>`, `internal/streams/add_consumer.go`), which name both sides, where MediaMTX's
names neither: an operator reads from the log line or the page what the viewer asked for and
what the stream has. go2rtc writes them only when no media at all matched; oxrtc writes them for
every refusal of codecs (an offer of nothing it sends, a stream none of whose sources has a
codec it lists, video unusable). Codec names as go2rtc prints them (`video:H264`,
`audio:OPUS`), each side cut at 16 names; the offer's side read from its receiving lines (rtx,
red, comfort noise and DTMF left out); a stream side not known yet is `none known yet`.

**Cost.** For a refused offer the stream's picture is subscribed to and dropped at once: the
camera the sound comes from, already running for it, or started on demand as for any viewer.
An offer of VP8 to a stream of sound alone gets an answer where go2rtc's fails.

Changes if: oxrtc sends VP8, VP9 or AV1 (a transcode to one of them), or a client is found that
needs the sound alone with its video line rejected.

**Amended 2026-11-23 (the D review):** the offer's side of a refusal is always the offer's own
list, also when the stream refused the subscription itself (no source with a codec wished): it
was rebuilt from the subscription's wish, which holds only what this server sends and, for an
unusable video, "any picture" - a VP8 and Opus offer to a G.711 microphone was refused `=>
audio:OPUS`, its VP8 left out (`a_refusal_by_the_stream_names_every_codec_of_the_offer` failed
before).

## D44 - MSE follows WebRTC's H.265 rule: one comparison in the core, read from the codec list's annex E strings (2026-11-23)

M5 item D (3), the owner's choice of 2026-11-23 on the F3 review's open line ("W7's H.265 profile
rule left open for MSE"). Until now `{"type":"mse"}` read a codec list by family: any `hvc1` or
`hev1` entry took the camera's H.265 whatever its profile, tier and level, as go2rtc does. Now the
camera's own H.265 goes to an MSE reader only when an `hvc1`/`hev1` entry of its list takes the
camera's profile, tier and level, else the stream's next source without H.265 (openIVR's H.264
transcode), exactly as a WebRTC offer since W7 (D41).

**One implementation** (`crates/core/src/h265_receiver.rs`, `h265_fallback.rs`): what a receiver
decodes is a `HevcReceiver` (profile space, profile or any, tier, level or none), read from a WebRTC
payload type by `oxrtc-webrtc` (`hevc::offered`) and from an MSE codec string by the core
(`HevcReceiver::from_codec_string`); `HevcReceiver::takes` is the one comparison, and
`h265_fallback::subscribe` the one subscription path (the first subscription held until the
fallback has tracks, the camera's H.265 anyway when the stream has no other source, a reader that
says nothing - `stream.mp4`, an empty MSE list - not checked). WebRTC's `server.rs` and MSE's
`fmp4.rs` both call it; the copy W7 had in `webrtc/src/hevc.rs` (`HevcPtl::takes`, `refused`,
`of_track`) is gone.

**The string** (ISO/IEC 14496-15 annex E.3, RFC 6381): `hvc1`/`hev1`, then each optional and in
order the profile space (none, `A`, `B`, `C`) with `general_profile_idc`, the 32 compatibility
flags in hexadecimal in reverse bit order, the tier (`L`/`H`) with `general_level_idc`, and up to
six constraint bytes in hexadecimal. A malformed string, one over 64 bytes or a family alone names
no limit (`HevcReceiver::ANY`): go2rtc's behaviour for it, and what the browser's
`isTypeSupported` already vouched for. The parser is new input from a page: fuzz target
`hevc_codec_string` with its proptest twins, and `ws_message`'s harness checks that every entry
of an `mse` list is read so.

**The rule, for both** (`takes`): the same profile space; the receiver's profile, or one the
camera's compatibility flags say its picture conforms to too (libx265's Main is flagged Main 10
compatible, so a Main 10 decoder takes it: for WebRTC, str0m is told the payload type's profile
and tier at the camera's level, which it matches exactly), and any Main picture for a Main 10
receiver (ITU-T H.265 A.3.3: a Main 10 decoder decodes every bitstream of `general_profile_idc` 1
or 2 or compatibility flag 1 or 2; amended in the D review, where a Main camera flagged Main alone,
as a hardware encoder may write it, was sent the transcode for a list or an offer of Main 10
alone, and go2rtc sends its H.265); a tier no higher (a High-tier decoder
takes the Main tier); a level no higher than the receiver's when it names one (D41 as amended:
none named is no ceiling).

**What it costs, measured**: go2rtc's player lists the fixed `hvc1.1.6.L153.B0` (Main, level 5.1)
whatever the browser decodes, so with it a Main 10 camera, or one above level 5.1, now gets the
transcode where it got its H.265 (and where go2rtc 1.9.15 sends its H.265, which a browser that
cannot decode it fails at `addSourceBuffer` or plays as it can). The owner's cameras (Main, level
5.0) are unchanged: their H.265 goes to go2rtc's player as before. A page that lists what its
browser decodes (`hvc1.2.4.L153.B0` for Main 10) gets the camera's H.265.

Changes if: go2rtc's player comes to list the browser's real profiles, or a browser is found
whose `isTypeSupported` answers yes for a string it then cannot play.

## D45 - One memory budget for every reader that writes over HTTP or a WebSocket, sized from what the host gives the process (2026-11-23)

M5 item D (6), the owner's answer to the F4 review ("queue=recorder lets one response hold 32 MiB,
64 responses 2 GiB"): something smarter than a fixed cap, that adapts to the host.

**One budget, process-wide** (`crates/core/src/memory.rs`, `MemoryBudget`, set on the hub by the
application): every queue of a reader that writes over HTTP or a WebSocket draws from it -
`stream.mp4`, `frame.mp4` and MSE now (`ConsumerTag::pooled`), HLS and MJPEG when they come. No
other accounting of buffered bytes existed: each consumer's queue counted its own (`queue.rs`),
R44 is about ICE servers, and `ffmpeg.max_memory_mib` bounds FFmpeg's address space, not the
server's queues. WebRTC sessions and RTSP readers keep their class's bounds alone: they are not
HTTP or WebSocket egresses, and a recorder's hole is worse than its memory.

**Its size** (`size_from`): one eighth of what the process may still take - the smaller of the
control group's limit less its use and `MemAvailable` (`crates/core/src/host_memory.rs`: cgroup v2
`memory.max`/`memory.current` in the process's own group then at the namespace's root, cgroup v1
`memory.limit_in_bytes`/`memory.usage_in_bytes`, `/proc/meminfo`; on Windows and macOS the
physical memory and what is available of it, from `sysinfo`, already in the lock file as the
tests' process lister, so no new crate and no code of ours calling the system) - plus what the
budget holds itself (its own bytes are not taken from it twice), never more than an eighth of the
limit, never less than 32 MiB; read again every 5 s, so it shrinks under memory pressure and grows
back. `api.output_memory` sets another share (`"25%"`) or a size (`256`, in MiB, or `"256MiB"`,
`"2GiB"`), the size kept unless the process cannot take it. The default, with numbers: a 4 GiB
Raspberry Pi 5 with openIVR beside (about 2.3 GiB available) 295 MiB; an 8 GiB one (about 6.6
GiB) 825 MiB; in Docker with `--memory 2g` at most 256 MiB; the owner's PC (128 GiB, about 97
available) about 12 GiB, where the classes' own caps (8 MiB live, 32 MiB recorder a reader) bound
first. One eighth because openIVR, its FFmpeg children and the kernel's page cache need the rest
on the 4 GiB Pi, and a quarter of it is still dozens of a camera's groups of pictures.

**Each reader's bound** (`ConsumerQueue::limit`): the smaller of its class's cap and its fair share
(the budget over the readers drawing from it), never below one group of pictures of its stream
(the channel counts the bytes of each group, `last_group_bytes`), so that a reader is never cut
so short that it skips at every group for ever. **When the budget is full** all the same (many
readers' groups), the reader furthest behind - the one whose oldest frame has waited longest -
drops what it holds and resumes at its next keyframe, by the queue's own rule (its `dropped`
counter, `mp4 reader fell behind the stream` or `mse reader fell behind the stream` in the log, and
`output memory budget full` once per 10 s); the source never waits, and a reader that keeps up
holds next to nothing and is never chosen. The count is an upper bound of the memory held: readers
of one source share its frames, which each queue counts as its own.

**Shown** in `GET /api` beside the version (go2rtc adds its own facts there too):
`"output_memory": {"budget", "held", "readers", "skips", "limit", "available", "source"}`.

**Measured** (`target/d-work/stall/` of the M5 lane's report D.md): 20 readers of a 4 Mbit/s 720p
camera with 6.4 s groups that connect, ask and read nothing (a 4 KiB receive window), 60 s, the
server's own resident memory:

| Server, 20 stalled readers | RSS before | peak in 60 s | at 60 s | readers |
|---|---|---|---|---|
| go2rtc 1.9.15, `stream.mp4` | 21.9 MiB | 47.4 MiB | 47.4 MiB (105.7 once their buffers are read) | 20 kept |
| MediaMTX 1.21.3, RTSP over TCP (it has no `stream.mp4`) | 39.2 MiB | 45.2 MiB | 41.9 MiB | 0: cut after its `writeTimeout` (10 s; `writeQueueSize` 512 packets a reader) |
| oxrtc before (9d31d2a), `stream.mp4` | 11.9 MiB | 21.0 MiB | 20.2 MiB | 16 (4 refused `503`: 16 per address) |
| oxrtc before, `queue=recorder` | 12.0 MiB | 22.6 MiB | 20.2 MiB | 16 |
| oxrtc with the budget (12 GiB on this PC), `stream.mp4` | 14.4 MiB | 23.3 MiB | 22.7 MiB | 16; `held` 6.4 MB |
| oxrtc, `queue=recorder`, `api.output_memory: 64` | 14.4 MiB | 24.2 MiB | 22.7 MiB | 16; `held` 61 MB of the 64 MiB, no skip needed |

The resident memory stays low on oxrtc because the readers of one source share its frames; what
the budget counts (`held`) is each queue's bytes as its own, an upper bound: the 64 MiB budget held
16 recorders at 61 MB where, alone, they could count 16 x 32 MiB.

Changes if: a reader of another egress needs the pool (WebRTC sessions in thousands), or the
shared frames are counted once (an exact figure instead of an upper bound).

**Amended 2026-11-23 (the D review):** what a group uses is its working set, as `docker stats` and
the kubelet count it: `memory.current` less `memory.stat`'s `inactive_file` (cgroup v1:
`memory.usage_in_bytes` less `total_inactive_file`). Read as used whole, the page cache charged to
openIVR's container (`mem_limit: 4g`, `docker-compose.yml`) by the recordings it writes - cache the
kernel gives back before it refuses the group anything - put the group's use at its limit minutes
after the first recording, and the budget at its 32 MiB floor for good. And cgroup v2 is read from
the process's own group up through each parent to the namespace's root, the tightest kept (the
smallest limit, the least left under any): a limit set on a parent (a systemd slice's `MemoryMax=`,
a pod's) was missed, read as the machine's memory. `a_group_s_page_cache_is_not_read_as_used` and
`a_limit_on_a_parent_group_is_read` (fixtures `recording_v2`, `recording_v1`, `slice_v2`) failed
before. A stream with no picture gives its readers no floor: it has no group, and its byte count,
never reset by a keyframe, grew with every frame until a microphone's stalled reader was bounded
by its class's 8 MiB alone, not its share (`a_stream_without_a_picture_gives_its_pooled_readers_no_floor`
failed before: 4.0 MB held against an 0.8 MB share); a new track list starts the count again.
The line `consumer fell behind its queue budget` of a pooled reader names its share of the budget
too (`output_share=`, 0 for a reader that draws from none): it named its class's `max_bytes`
alone, 32 MiB for a recorder, a bound a reader cut at its share never reached
(`a_pooled_reader_that_falls_behind_is_told_its_share_of_the_output_budget` failed before).

## D46 - Motion JPEG: go2rtc's wire, a stream's own pictures passed on, any other stream encoded once by FFmpeg on its standard output; `frame.jpeg` shaped, and cached per shape (2026-11-24)

M5 F5. go2rtc serves Motion JPEG three ways (its `internal/mjpeg`): `GET /api/stream.mjpeg`
(multipart, for an `<img src>`, VLC, ffplay, FFmpeg, Home Assistant's MJPEG camera), the
`{"type":"mjpeg"}` message of `/api/ws` (its player's `mjpeg` mode, which sets each picture
as the `<video>`'s poster: the mode of a browser with neither WebRTC nor MSE, Safari on an
iPhone before iOS 17.1), and `GET /api/frame.jpeg` (one still, with `width`/`w`,
`height`/`h`, `rotate`, `cache`). MediaMTX 1.21.3 has none of them (no route of its writes
`multipart/x-mixed-replace` or `image/jpeg`).

**The wire is go2rtc's, byte for byte** (`crates/api/src/mjpeg.rs`): `Content-Type:
multipart/x-mixed-replace; boundary=frame`, each part `--frame`, `Content-Type: image/jpeg`,
its `Content-Length` (what VLC, FFmpeg's `mpjpeg` demuxer and Home Assistant read a part by),
the picture, a line end; `Cache-Control` and `Pragma: no-cache`. One difference, of order only:
go2rtc writes each part's delimiter before it, so a browser shows a picture once the next one's
delimiter has come (Chromium issue 527446, which go2rtc's writer names); oxrtc writes the
delimiter after each part, the same bytes on the wire, and a browser shows each picture as it
comes. go2rtc also sends `Connection: close`, which oxrtc leaves out (HTTP/1.1 keeps the
connection to the body's chunked end either way).

**Where the pictures come from.** A stream with a Motion JPEG source (an MJPEG camera over RTSP,
depacketised by RFC 2435 in the RTSP client, retina's `codec/jpeg.rs`, or over HTTP; or a configured
`ffmpeg:<stream>#video=mjpeg`) has its pictures passed on as they came, never decoded; a
picture with no Huffman tables (an `AVI1` webcam's) gets the standard ones written in
(`oxrtc_core::jpeg::with_huffman_tables`, go2rtc's `FixJPEG`), for the decoders that do not assume
them (not today's browsers: amended below). A stream with
none, go2rtc 1.9.15 answers an empty `200` (measured: `Content-Length: 0`) - its README asks the
operator to add `ffmpeg:camera1#video=mjpeg` beside the camera, an FFmpeg that runs whenever the
stream does. oxrtc makes that source itself, on demand: the stream's **derived** stream
`ffmpeg:<stream>#video=mjpeg` (`Hub::derived`, `crates/core/src/derived.rs`), one FFmpeg for every
Motion JPEG reader of the stream (`/api/ws` and `stream.mjpeg` alike), started by the first and
stopped after the slot's idle grace once the last has gone, as the H.264 transcode's lifecycle;
the key is fixed per stream, so its readers never start a second one. An operator who writes
go2rtc's line keeps it: it is then a source of the stream, read as its own JPEG.

**FFmpeg's pictures on its standard output, not over RTSP** (`crates/ffmpeg/src/pipe.rs`).
`#video=mjpeg` alone takes go2rtc's `output/mjpeg` template (`-f mjpeg -`), and oxrtc cuts the byte
stream into its pictures (`oxrtc_core::jpeg::Splitter`, fuzzed as `jpeg_split`), each published
as a keyframe at 90 kHz on the wall clock. Published back over RTSP, as every other transcode,
it could not be: RTP/JPEG (RFC 2435) carries no side over 2040 pixels and only the standard
tables, which FFmpeg's encoder does not write by default (R42). `#video=mjpeg` beside another
`#video` or any `#audio` is refused in plain words, as go2rtc pipes it only alone.

**Each reader** is a pooled consumer (D45), named `mjpeg` in `/api/streams` as go2rtc names it:
its queue bounded by its share of the output memory budget; a reader slower than the stream
loses pictures - each stands alone, so it resumes at the next one - counted (`dropped`) and
logged once per 10 s. go2rtc's consumer has no such bound. `stream.mjpeg` shares the bounds of
`stream.mp4`'s responses (64 at once, 16 per address, `503`), and its 75 s silence bound; the head
waits for the first picture, so a stream that cannot make one is a `500` with the reason, where
go2rtc's head is sent at once and its body stays empty. No frame-rate cap: go2rtc has none on the
route (its `#raw=-r 10` belongs to the source line, and a configured `ffmpeg:` source keeps it).

**`frame.jpeg`** (`crates/api/src/snapshot.rs`, `crates/ffmpeg/src/stills.rs`): go2rtc's `width`/`w`,
`height`/`h` (a side alone keeps the picture's shape) and `rotate` (`90`, `180`, `270`, `-90`;
after the scale, go2rtc's `transpose` filters), measured equal in size on both servers for every
shape of an H.264 stream. For a Motion JPEG stream go2rtc ignores all three (measured: 1280x720
whatever is asked); oxrtc passes the picture on when it is asked as it is and encodes it again
otherwise. `cache=<Go duration>`, as go2rtc: a picture made for a request that asked for a cache
is kept, and served to a request whose cache is longer than its age; a request without one
always makes a picture. go2rtc keeps one per stream and serves it whatever the size asked (its
README says so; measured: `cache=10s&width=320` answered its 1280x720 picture); oxrtc keeps one
per stream and shape (64 pictures and 32 MiB at most), so a picture of another size is never
served. What cannot be read (`rotate=45`, `width=0`, `cache=abc`) is a `400` naming the
parameter before any work, as `width` was since R12; go2rtc ignores it and answers the plain
picture.

**Measured** on the release build (F5's report, `f5-work/`: go2rtc 1.9.15 as the camera, a
1280x720 test pattern at 25 pictures a second as H.264 and as an MJPEG stream over RTSP; a
second go2rtc 1.9.15 reading the same camera with its README's `ffmpeg:h264#video=mjpeg`):

| | oxrtc | go2rtc 1.9.15 |
|---|---|---|
| `stream.mjpeg` of the H.264 stream, head and first picture | 418 ms (its transcode starts) | 3.5 s |
| of the MJPEG camera | 62 ms, the camera's pictures as they came | 68 ms |
| of a stream with no JPEG source and no line for one | its transcode | an empty `200` |
| `ffmpeg -xerror` of 100 pictures | 100, no error, 4.1 s | 100, no error, 7.8 s (H.264) / 4.1 s |
| `<img src>` in headless Edge | 1280x720, pictures changing, first in 436 ms (110 ms for the camera's) | the same, first in 3.3 s (105 ms) |
| go2rtc's player, `mode=mjpeg` | 35 pictures shown in 4 s, the first after 537 ms | 17, the first after 2.5 s |
| its `mode=hls,mjpeg` (the `hls` error moves it on) | 39 in 4 s | 39 in 4 s |
| server CPU, 1 and 5 readers of one stream (alternated, 3 passes of 15 s) | 1.2 % and 2.8 % of a core | 1.9 % and 3.3 % |
| FFmpeg processes for 5 readers, and after they left | 1, then 0 | 1, then 0 (its source line) |
| no credential, from loopback | `401` | served (go2rtc does not ask loopback) |
| `src` absent | `400 src is required` | `404` |

Not done here: go2rtc's `POST /api/stream.mjpeg?dst=` (a multipart MJPEG pushed into a stream,
an ingest: with the other ingests of M6), `stream.ascii` and `stream.y4m` (M6, PARITY), and
`hardware`/`hw` on `frame.jpeg` (the hardware encoders, R12).

Changes if: the owner wants an unreadable `frame.jpeg` parameter ignored as go2rtc ignores it
(a client written against go2rtc that sends one would then get its picture); or a reader asks
for a frame rate of its own (a per-request cap, which go2rtc does not have).

Amended (F5 review, 2026-11-24). **Tables:** the claim that Firefox needs them was not measured,
and is not so: headless Firefox 159 (Playwright's build) and Edge draw a webcam's pictures with
no Huffman tables in an `<img>` of `stream.mjpeg`, from go2rtc 1.9.15 (which passes them on
without) and from oxrtc alike. The tables stay written in, for the decoders a still is handed to
that do not assume them (go2rtc's `FixJPEG` names Rust's jpeg-decoder and pion's mediadevices);
go2rtc writes them only into an `AVI1` picture's `frame.jpeg`, oxrtc into every picture without
them, on every route. **`frame.jpeg` of a Motion JPEG stream asked as it is** was passed on as it
came, where go2rtc's `FixJPEG` encodes a picture again when its first segment is neither a
quantization table nor a JFIF, Exif or `AVI1` one (an mjpg-streamer camera's, a comment first),
so that the tools that sniff a file's type (Python's `imghdr`) and image services such as
Telegram's take it (go2rtc's comment: `IMAGE_PROCESS_FAILED`), and serves it as it came if that
fails: oxrtc now does the same (`oxrtc_core::jpeg::wants_encoding_again`, FFmpeg's still path,
the picture as it came should FFmpeg fail); an RTSP camera's picture, rebuilt by retina with its
quantization tables first, is never encoded again. Proven by
`a_jpeg_frame_whose_header_file_tools_cannot_read_is_encoded_again` (real FFmpeg; failed before),
`a_jpeg_frame_that_cannot_be_encoded_again_is_served_as_it_came` and
`a_still_is_encoded_again_when_go2rtc_would_encode_it_again`. **CPU, measured again** (same setup,
3 rounds alternated, each reader reading as fast as it can, the PC shared with other work): the
server's own CPU for 1 and 5 readers 1.97 % and 4.09 % of a core against go2rtc's 2.74 % and 3.71 %
(the passes spread from 3.1 to 4.8 % for oxrtc's five), while oxrtc's readers got the camera's 25
pictures a second and go2rtc's 20 (its README's line): per picture delivered 0.079 % and 0.033 %
against 0.137 % and 0.037 %. Five readers are thus a tie in absolute terms and a win per picture,
not the clear win the table above reads as; the bench's MJPEG scenario is to count CPU per picture
delivered (R34). go2rtc's player in `mode=mjpeg` was run again in headless Firefox 159 and Edge:
35 and 36 pictures shown in 4 s from oxrtc, the first after 511 and 435 ms, against go2rtc's 13 and
14 after 2.7 and 2.8 s.
