# Architecture

## The shape

```
              ┌──────────────────── apps/oxrtc ─────────────────────┐
              │  config · wiring · signals · logging                   │
              └──────┬──────────────┬──────────────┬───────────────────┘
                     │              │              │
   control plane  ┌──▼───┐       ┌──▼──────┐    ┌──▼──────────┐
   (auth always)  │ api  │       │ webrtc  │    │ rtsp-server │  egress
                  └──┬───┘       └──┬──────┘    └──┬──────────┘
                     │  subscribe / preload / snapshot │
                  ┌──▼────────────────────────────────▼──┐
                  │                 core                 │  no I/O
                  │ Hub · Stream · Slot · Channel · GOP  │
                  └──▲────────────────▲──────────────────┘
                     │ ProducerSink   │
              ┌──────┴──────┐  ┌──────┴─────┐
              │ rtsp-client │  │   ffmpeg   │  ingress
              │  (retina)   │  │ (confined) │
              └─────────────┘  └────────────┘
```

Ingress crates implement `SourceFactory`/`SourceDriver` and publish frames into
a `ProducerSink`. Egress crates `subscribe` to a `Stream` and receive `Event`s.
Neither side knows the other; the core is the only meeting point. A relay
(`stream:`, or a loopback URL of our own RTSP port) is not ingress: it is an
alias the core resolves when a consumer subscribes (below).

## The core (`crates/core`)

- **Hub**: named streams. `upsert` with identical sources is a no-op (viewers
  keep watching - go2rtc tears the stream down and cuts them); a different
  source list closes the old stream and its subscriptions end with `Closed`.
- **Stream**: an ordered list of **slots**, one per source. A consumer's `Want`
  (video and/or audio, codec lists) is resolved per kind: the first slot that
  offers an accepted codec wins. Offers come from the running connection's
  tracks, else from the driver's `declared()` output, else the slot is probed.
  A slot that only declares, with another candidate after it, is started and
  passed over at its first failed attempt, as go2rtc falls through its sources
  (openIVR's de-duplicated sound transcode exits for audio it cannot
  de-duplicate; the plain transcode after it serves). A sole candidate keeps
  its chance until the deadline: a camera may refuse one connection and take
  the next. A source that reads this server (an `ffmpeg:` of a stream or of
  the relay URL: `SourceDriver::local`) does not: its failure is this
  server's own answer, which a retry repeats. Every wait shares one deadline,
  `probe_timeout` (8 s) from the subscribe call, under WHEP's 10 s answer
  deadline.
- **Alias** (`local.rs`, `select.rs`, D14): a `stream:<name>?<query>` source,
  and a loopback RTSP URL on our own port, which the RTSP factory turns into
  one. It has no driver, channel or supervisor. At selection it stands for
  the sources of the stream it names, the consumer's wish narrowed by its
  query (`?video` passes no sound), so the candidates of a media are a flat
  list - openIVR's `camera_N` sound is `camera_N_rec`'s transcode itself -
  and the consumer reads the upstream source's own channel, GOP cache and
  timeline: no copy, one backoff, one idle grace. The name is looked up at
  each selection (a stream named before it exists is read once it does). A
  subscription watches the streams its routes were resolved through: one
  replaced, those routes are resolved again in the subscribed stream and
  reach its successor (frames resume on a keyframe); one removed, or nothing
  serving there any more, the subscription ends with `Closed` when it reads
  nothing else, and otherwise goes on with its other media and asks for the
  lost ones again every `backoff_max` (a camera's picture is not ended with
  the stream that lent it its sound, which may come back). `Hub::upsert`
  refuses a stream through whose aliases one media would come back to it
  (`SpecError::Cycle`, checked under the write lock, each media followed
  apart with its wish narrowed by the queries on the way: `a`'s sound read
  from `b` and `b`'s picture from `a` is no cycle). Resolving one media
  enters a stream again only with another wish or from nearer the top, lists
  a source once per wish, and follows at most `MAX_ALIAS_DEPTH` (8) aliases
  in a row, `MAX_CANDIDATES` (64) candidates and `MAX_ALIAS_VISITS` (256)
  streams entered. A listing shows an alias as the source it reads there (`reads`),
  with the tracks it passes.
- **Slot**: demand counting and the supervisor. First demand starts the driver;
  last release stops it after `idle_grace`. A run that fails, ends, produces
  nothing for `connect_timeout` or nothing more for `stall_timeout` is
  restarted after exponential backoff with ±25 % jitter, capped at
  `backoff_max` (5 s: a camera that comes back is retried within 5 s; 30 s
  cost 18 s of recording after a 60 s outage in the bench's R01); backoff
  resets after `healthy_after` of good media. Refused credentials have their
  own, slower one (`auth_backoff_initial` 30 s doubling to
  `auth_backoff_max` 10 min): a changed password retried every 5 s would lock
  the account of a camera that counts failed logins, for every client.
- **Channel**: one producer, many consumers. Each consumer has a **queue of
  its own** (`queue.rs`, D15) that carries track lists and frames in order,
  the frames shared by `Arc`. It is bounded by bytes (each item's payload
  plus `ITEM_OVERHEAD`, so tiny frames are bounded too) and by the age of
  its oldest item, by the consumer's class (`ConsumerTag::class`):
  `Limits::live_queue` (8 MiB, 2 s: WebRTC viewers, stills, preloads;
  `webrtc.queue_*`) or `Limits::recorder_queue` (32 MiB, 10 s: RTSP
  readers, a segmenter or a transcode; `rtsp.queue_*`). An RTSP reader is a
  recorder unless its URL says `?queue=live` (D18: a detector that shows
  what it reads); a viewer's `User-Agent` and a reader's are kept, cleaned
  and bounded, in the consumer's tag (`ConsumerTag::user_agent`) and listed
  by `/api/streams`. The producer
  enforces the budget when it publishes: beyond it, the queue keeps what follows its latest keyframe if that fits (the
  consumer resumes at once, not at the camera's next keyframe up to 6 s
  away), else drops every frame (the latest track list is kept); a source
  without video loses its oldest frames. The consumer gets `Discontinuity`,
  its video resumes on a keyframe, the drop counts in its `dropped`, and it
  is logged (`consumer fell behind its queue budget`, a warning for a
  recorder, debug for a live consumer, at most once per 10 s per consumer
  with the count). The channel also keeps the **current GOP** (up to
  `gop_cache_bytes`, `api.gop_cache_mib`): a new subscriber receives it
  first, so it decodes at once, and a viewer's keyframe request is answered
  from it (below). A group that outgrows it is dropped from the cache until
  the next keyframe, and said once per source (a warning with the group's
  size and the bound; nothing else shows it). The hub's `Limits` come from
  the file (`apps/oxrtc/src/app.rs`, `hub_limits`): the streams' own in
  `api:` (D16), each queue budget in its egress's section, every key left
  out at its default.
- **Timeline** (`timeline.rs`, D12): every published frame's `pts` is put on
  one timeline per track before any consumer sees it, so every egress reads
  the same time. A source's own timestamps restart with each connection
  (retina counts from each session, a transcoder from 0); on the timeline each
  **run** of a source is anchored to the wall clock - its first frame of a
  track is placed at when it was received, counted from the hub's epoch, one
  time base for every source of the hub - and keeps the source's own steps
  within the run (a camera that delivers in bursts keeps its regular timing).
  A step back of more than 1 s, or forward of more than 10 s beyond the wall
  clock between two frames, is a restart inside the run, anchored again. No
  time is more than those 10 s ahead of its frame's arrival: a source whose
  clock runs fast in smaller steps is held there, and so is the lead a run
  ends with (a transcode through the cached GOP) when the next is anchored
  after it, which quick restarts would otherwise add up. An
  anchor never lands at or before the track's latest time while a consumer
  is attached, so a reader across a restart never goes back. A relay reads
  the frames of the stream it names, so it has that stream's time. The
  timelines outlive the source's connections and its track lists (a new list
  continues each kind, converted to its clock rate; an RTSP reader, described
  in the old rate, is ended instead and describes again).
- **Subscription**: merges one or two slots (e.g. video from the camera, Opus
  from a transcoder) into one event sequence. The first event is always
  `Tracks`; frames that arrive before every route is resolved are held
  (bounded) and released in order. A route whose source fails an attempt
  before delivering tracks is given up for that first list, so a camera
  without a microphone, whose sound transcode exits at once, still plays at
  once; the list goes out with what is resolved at the deadline otherwise.
  A subscription with nothing resolved ends with `SubEnd::NoTracks`: at the
  deadline, or at once when every route failed and reads this server (a
  preload of a sound that cannot exist answers 500 in milliseconds, and
  openIVR then holds the video).

Frames are access units; H.264/H.265 payloads are length-prefixed NAL units,
validated on publish. Timestamps are in the track's clock rate, on the
timeline above; `Frame::received` is when the frame entered oxrtc, kept
through relays.

## Data path and copies

A camera frame is allocated once (by the depacketizer) as `Bytes` and shared by
`Arc` with every consumer, through every relay too: one channel and one GOP
cache per source, never one per relay (each consumer's queue holds pointers
to the same frames) (`a_relay_hands_on_the_upstream_frames_themselves`).
Egress packetizes per consumer (RTP for RTSP, str0m for WebRTC). There is no
decode in the pass-through path.

Neither egress keeps a backlog of its own: each reads its subscription only
when it can send, so a consumer that cannot keep up leaves its frames in its
core queue, under its class's budget, and nowhere else. The RTSP connection
takes the next event only while its write queue (`outbox.rs`) holds less
than 1 MiB and has places left beyond the 8 kept for responses; the writer
wakes it after each write. A reader that pauses (a segmenter rotating its
file, a process descheduled) thus has its 10 s or 32 MiB, and the write
queue never drops a frame for its size. A WebRTC session reads nothing until
ICE and DTLS are connected: what comes meanwhile waits in the live queue
(the session's own queue of 512 frames is gone). One that takes longer than
2 s to connect finds its queue cut, often with no keyframe left in it (the
cached group it started from is not in the queue): it resumes from the
cached group of pictures (below) instead of waiting for the camera's next
keyframe, which on a stream it cold-started left it with nothing to show
(`a_session_that_connects_past_its_budget_resumes_from_the_cached_gop`).

Both egresses send the timeline as their RTP time (plus a random offset for
RTSP) and have no continuity logic of their own. The RTSP egress precedes each
track's first packet with an RTCP sender report dated by that frame's
`received` - a frame of the cached GOP by when it entered oxrtc, not by when
the reader got it - and derives every later report from that one along the
timeline, not from when the last packet left. A first frame the reader's queue
drops takes its report with it, and the next frame of that track goes after a
report of its own; a periodic report dropped with a frame goes with the next. WebRTC hands str0m the frame's
`received` as its wall clock, and str0m reports from the last frame written.

## WebRTC: the paths to a viewer

`crates/webrtc` answers each offer with every path it has, and str0m's ICE
picks one:

- **Host candidates** on one UDP socket shared by every session
  (`webrtc.listen`, every interface address plus `webrtc.candidates`). A
  datagram is routed to its session by its route (how it arrived - this
  socket, a relay, a TCP connection - and the peer's address); from a route
  no session knows, only a binding request is taken, handed to the session
  whose ICE user name fragment it names (`registry.rs`, a map), which
  learns the route if its `Rtc` accepts it. Anything else from an unknown
  route is dropped: a stranger's datagram is parsed once whatever the
  session count. A session keeps 8 routes: a STUN message it accepts on a
  route makes that route its newest, and past 8 it forgets the one its peer
  checked least recently, so the pair in use (consent-checked every few
  seconds) is never the one forgotten. A response to one of
  our own checks from a route not yet learned is dropped too: this server
  is always the controlled agent, so the nominated pair's route is learned
  from the peer's own check, and a triggered check answers the rest.
- **ICE-TCP passive candidates** (RFC 6544) at the same addresses, on a TCP
  listener at the UDP port: one connection per peer, frames of RFC 4571,
  claimed by the session whose `Rtc` takes its first frame.
- **A server-reflexive candidate** per `stun:` server of
  `webrtc.ice_servers`: the shared socket's address as that server sees it,
  asked through the shared socket at startup and every five minutes.
- **A relayed candidate** per `turn:` server (UDP): one allocation each,
  shared by every session, on a socket of its own (D13). The TURN client is
  Sans-I/O (`turn/allocation.rs`, as str0m is); a task per server drives it
  (`turn/driver.rs`), re-allocating after a failure with the core's
  jittered backoff (`turn/retry.rs`). A peer's data from the relay enters
  the same dispatch as the shared socket's,
  marked as arrived on the relayed address; a session's datagram leaves by
  the route of its source candidate (`socket.rs`: a TCP candidate's
  connection, a relayed candidate's TURN server, else the shared socket).
  Permissions for the IPs an offer lists are asked for while it is
  answered, so the viewer's first checks pass the relay.

A session with host candidates only is ICE-lite; one with a reflexive or
relayed candidate is full ICE, controlled (the browser controls).

**Keyframe requests** (RTCP PLI or FIR, `replay.rs`, R24, D15). A browser
that lost a packet of the picture asks for a keyframe; oxrtc has no upstream
to ask (retina takes no RTCP feedback, a transcode neither) and the camera's
next one can be seconds away. The session is sent the source's cached group
of pictures again (`Subscription::cached_video`: its video from the last
keyframe to the latest frame), which brings the decoder to the present at
once. A browser drops a frame older than one it decoded, so the replayed
frames get RTP times of their own, evenly spread after the last time the
session sent and before where the next new frame is expected (the last
time of the group plus the track's last step), dated by the last frame's
wall clock for the sender reports; the queued frames the replay repeats are
skipped (`TrackOut::repeats`). A camera's steps vary: a new frame that
comes earlier than that last step would go back before the replay's last
time, so it is written one tick after it (`TrackOut::time_of`), until the
frames' own times are ahead again. A track that sent nothing yet replays with
the frames' own times. A cut of the session's own queue (`Discontinuity`) is
answered the same way, under the same interval, as if the viewer had asked.
Not replayed: the sound; a group that is not cached
(no keyframe yet, or larger than `gop_cache_bytes`), longer than 1000
frames, or with more frames than ticks to spread them over (logged at
debug). One replay per session per second (`REPLAY_INTERVAL`); requests in
between are answered together at its end, since a group costs up to
`gop_cache_bytes` to send.

## Processes

One process. FFmpeg runs as child processes only when a transcode or a still
is needed, with an argument vector (never a shell), a cleared environment,
`kill_on_drop`, and its stderr drained into the log. A transcoder publishes its
output back into the local RTSP server on a one-time path token.

FFmpeg dies with oxrtc, a crash or a `SIGKILL` included (D5, R13):

- **Windows**: each FFmpeg is in a job object that kills its processes when
  its handle closes, which the system does when oxrtc dies. FFmpeg starts
  directly.
- **Linux**: FFmpeg starts through `oxrtc-exec` (`apps/oxrtc-exec`, a second
  binary shipped beside `oxrtc`), which confines itself, then `exec()`s
  FFmpeg - the same process, so every restriction stays: `PR_SET_PDEATHSIG`
  (`SIGKILL`) and a check that its parent is still oxrtc (else it ran after
  oxrtc died and nothing would kill it); `PR_SET_NO_NEW_PRIVS`; `RLIMIT_AS`
  4 GiB (`ffmpeg.max_memory_mib`), `RLIMIT_FSIZE` 0, `RLIMIT_NOFILE` 256,
  `RLIMIT_CORE` 0 (each lowered, never raised: a lower limit oxrtc inherited,
  an operator's `ulimit -S` included, stays), niceness +2; a Landlock
  ruleset (R29). The workspace forbids unsafe code, so this cannot be a
  `pre_exec` hook in oxrtc itself.
  oxrtc-ffmpeg builds the wrapper's command line (`oxrtc_exec::Plan`, one
  definition read back by the wrapper) and the policy
  (`crates/ffmpeg/src/confine.rs`).

The Landlock ruleset (ABI v9 asked, best effort: an older kernel enforces
the rights it knows, `landlock=partial`; one without Landlock runs FFmpeg
with the rest, and oxrtc warns once at startup). FFmpeg may read and execute
beneath `/usr`, `/bin`, `/sbin`, `/lib*`, `/nix/store`, its own directory
(through a symbolic link too) and `LD_LIBRARY_PATH`; read the loader's files,
name resolution's (`/etc/hosts`, `/etc/resolv.conf`, `nsswitch.conf`...),
certificates (`/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`), the time zone,
`/run/systemd/resolve`, its own `/proc` entry, `/proc/cpuinfo` and `meminfo`,
the CPU list in `/sys`, and `/dev/null`, `zero`, `random`, `urandom`, `full`;
open `/dev/null` for writing; connect to the name services' sockets
(`/run/systemd/resolve`, `/run/nscd`, `/run/avahi-daemon`). Nothing else:
not `/etc` as a whole (an operator may keep oxrtc's configuration there), not
`/proc` (oxrtc's environment), not `/home`, `/tmp`, `/var`, `/opt`; nothing
created, removed or renamed anywhere. Signals and abstract UNIX sockets are
scoped to the sandbox. Three of those need a recent kernel, and an older one
leaves them out (`landlock=partial abi=N`, the startup line names what is
missing, `Landlock::unenforced`): truncating a file FFmpeg can otherwise not
write (ABI 3, Linux 6.2), device ioctls (ABI 5, 6.10), and the signal and
abstract-socket scoping (ABI 6, 6.12) - below 6.12 a compromised FFmpeg can
signal oxrtc, which runs as the same user. The network is not restricted:
FFmpeg reads cameras and publishes back to oxrtc.

`PR_SET_PDEATHSIG` fires when the *thread* that started the process ends,
not the process (prctl(2)). Every FFmpeg is therefore started from one
thread that never ends (`crates/ffmpeg/src/spawner.rs`), on every platform,
and awaited on the caller's runtime; a start from a runtime thread that then
idles out (a blocking-pool thread, after 10 s) would otherwise kill FFmpeg
with it.

At startup oxrtc looks for `oxrtc-exec` beside its own executable and runs it
once with `--check` (the same confinement, nothing run): `INF [ffmpeg]
confined (..., landlock=full)`, or `landlock=partial abi=4); not enforced by
this kernel: device ioctls (...), signal and abstract-socket scoping (...)`,
a warning for a kernel without Landlock,
an error if the wrapper refuses (every FFmpeg is then refused the same way,
as its source's error: confined or not at all). Missing on Linux, it warns
that FFmpeg runs unconfined and outlives a killed oxrtc, and starts FFmpeg
directly. A set-user-ID launcher of FFmpeg (snap's) cannot run under
no-new-privileges: use the distribution's FFmpeg or a static build.

## Listeners: limits and deadlines

Every listener (the API, the RTSP server, the ICE-TCP listener) and the WHEP
session registry has a total and a per-address bound (`oxrtc_core::PerIpLimit`,
D9). A refusal at either bound is logged through `oxrtc_core::Refusals`: the
first one at once, the next ones at most once a second with their count
(`refused=`), so a connect flood cannot flood the log as well. The API's
total is the exception that refuses nothing: a connection over it waits in
the kernel's accept queue, and reaching it is logged through the same
throttle (`api: connection limit reached, new connections wait`, `reached=`).

Two clocks bound a peer's time, besides the size caps:

- **Silence** restarts at every byte: the API's 75 s idle timeout (reads and
  writes), the RTSP `session_timeout` (60 s).
- **A whole request** does not. RTSP: a message (request or interleaved
  frame) must arrive within `request_timeout` (10 s) of its first byte,
  counted from that byte, or from when the message before it was handled;
  beyond, `408` and the connection closes (`conn.rs`). API: a request's head
  must arrive within `HEAD_TIMEOUT` (10 s) of its first byte (`listener.rs`:
  the connection's reads start the clock after each answer, and the router's
  first layer stops it when hyper hands it the request; that layer also says
  when the router has its answer, since a `100 Continue` is a write that is
  not one, and what follows it is the same request's body); the body and the
  answer are under the routes' deadlines (10 s, 60 s for `/api/webrtc` and
  `/api/preload`). hyper's own `header_read_timeout` is not used: it needs a
  timer `axum::serve` does not install. ICE-TCP: a connection no session
  claims within 10 s is closed, then it lives as long as its session.

## Concurrency

Tokio multi-threaded runtime. Locks are `std::sync` and never held across an
`.await`. Every spawned task is owned by a cancellation token: a slot's
supervisor, a WebRTC session, an RTSP connection. Shutdown cancels the root
token, closes the hub, and waits for the tasks with a deadline.

## Releases

Two binaries ship: `oxrtc` everywhere, and on Linux `oxrtc-exec` beside it
(see "Processes"). `scripts/release_local.py` builds them for linux-amd64,
linux-arm64 and windows-amd64 and packs each platform's archive with the
licences, the README, `THIRD_PARTY_NOTICES.txt` (cargo-about's report
rendered by `scripts/release_notices.py`) and a CycloneDX SBOM per binary;
`release.yml` runs it per platform (D17). A Linux binary built elsewhere is
checked by reading its ELF headers (`scripts/release_elf.py`), since it
cannot run there: glibc 2.35 is the floor, every loadable segment is aligned
to 64 KiB on arm64 (the Raspberry Pi 5's kernel uses 16 KiB pages), and no library outside glibc is
needed - aws-lc, the only C code (R15), is linked in. The Windows binary links
the C runtime in (`+crt-static`) and its PE imports are read the same way
(`scripts/release_pe.py`): Windows' own DLLs only, no `VCRUNTIME140.dll`.
