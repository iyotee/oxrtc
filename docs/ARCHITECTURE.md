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
                  └──▲────────────────▲──────────────▲───┘
                     │ ProducerSink   │              │
              ┌──────┴──────┐  ┌──────┴─────┐  ┌─────┴───────┐
              │ rtsp-client │  │   ffmpeg   │  │ stream: (in │  ingress
              │  (retina)   │  │ (sandboxed)│  │   core)     │
              └─────────────┘  └────────────┘  └─────────────┘
```

Ingress crates implement `SourceFactory`/`SourceDriver` and publish frames into
a `ProducerSink`. Egress crates `subscribe` to a `Stream` and receive `Event`s.
Neither side knows the other; the core is the only meeting point.

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
  the next. A source that reads this server (`stream:`, a loopback relay, an
  `ffmpeg:` of a stream or of the relay URL: `SourceDriver::local`) does not:
  its failure is this server's own answer, which a retry repeats. Every wait
  shares one deadline, `probe_timeout` (8 s) from the subscribe call, under
  WHEP's 10 s answer deadline.
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
- **Channel**: one producer, many consumers. A `tokio::sync::broadcast` ring of
  `ring_capacity` items carries track lists and frames in order. A lagging
  consumer gets `Discontinuity` and its video resumes on the next keyframe. The
  channel also keeps the **current GOP** (up to `gop_cache_bytes`): a new
  subscriber receives it first, so it decodes at once.
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
  is attached, so a reader across a restart never goes back. A `stream:`
  relay keeps the time of the stream it reads (`ProducerSink::relay`). The
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
`Arc` with every consumer. Egress packetizes per consumer (RTP for RTSP,
str0m for WebRTC). There is no decode in the pass-through path.

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
  datagram is routed to its session by the address it came from, else
  offered to each session (STUN carries the session's user name).
- **ICE-TCP passive candidates** (RFC 6544) at the same addresses, on a TCP
  listener at the UDP port: one connection per peer, frames of RFC 4571,
  claimed by the session whose `Rtc` takes its first frame.
- **A server-reflexive candidate** per `stun:` server of
  `webrtc.ice_servers`: the shared socket's address as that server sees it,
  asked through the shared socket at startup and every five minutes.
- **A relayed candidate** per `turn:` server (UDP): one allocation each,
  shared by every session, on a socket of its own (D13). The TURN client is
  Sans-I/O (`turn/allocation.rs`, as str0m is); a task per server drives it
  (`turn/driver.rs`), re-allocating after a failure with backoff. A peer's
  data from the relay enters the same dispatch as the shared socket's,
  marked as arrived on the relayed address; a session's datagram leaves by
  the route of its source candidate (`socket.rs`: a TCP candidate's
  connection, a relayed candidate's TURN server, else the shared socket).
  Permissions for the IPs an offer lists are asked for while it is
  answered, so the viewer's first checks pass the relay.

A session with host candidates only is ICE-lite; one with a reflexive or
relayed candidate is full ICE, controlled (the browser controls).

## Processes

One process. FFmpeg runs as child processes only when a transcode or a still
is needed, with an argument vector (never a shell), a cleared environment,
`kill_on_drop`, and its stderr drained into the log. A transcoder publishes its
output back into the local RTSP server on a one-time path token.

## Concurrency

Tokio multi-threaded runtime. Locks are `std::sync` and never held across an
`.await`. Every spawned task is owned by a cancellation token: a slot's
supervisor, a WebRTC session, an RTSP connection. Shutdown cancels the root
token, closes the hub, and waits for the tasks with a deadline.
