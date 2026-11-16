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
consumers; an `ffmpeg:` transcode reads over loopback RTSP, so it is a
recorder, and a transcode that falls behind (a loaded CPU) can lag up to
10 s before it is cut to a keyframe; a replay's frames are decoded in a
burst within one frame interval. Changes if: a consumer class needs its own
budget (a live transcode), which needs the reader to say what it is (D18). The
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
could become `live`), or a transcode needs a class of its own (R23: the
`ffmpeg:` driver could add the key to the URL it reads).

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
  is longer than 16 KiB (a relayed WebRTC packet is under 1.5 kB). So the
  buffer holds one message and one read (4 KiB) at most, and a server that
  lies closes the connection instead of growing it. Fuzzed (`turn_stream`,
  with the framer's own invariant: the same messages whatever the reads).
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
