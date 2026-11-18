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
       ┌──────────┴─┐  ┌────────┴───┐  ┌─┴──────────┐
       │ rtsp-client│  │ http-client│  │   ffmpeg   │  ingress
       │  (retina)  │  │  (hyper)   │  │ (confined) │
       └────────────┘  └────────────┘  └────────────┘
```

Ingress crates implement `SourceFactory`/`SourceDriver` and publish frames into
a `ProducerSink`. Egress crates `subscribe` to a `Stream` and receive `Event`s.
Neither side knows the other; the core is the only meeting point. A relay
(`stream:`, or a loopback URL of our own RTSP port) is not ingress: it is an
alias the core resolves when a consumer subscribes (below).

Two crates are shared by the protocol crates rather than being protocols:
`crates/net` (the camera clients' TCP connect and TLS) and `crates/depacketize`
(RTP to frames, with retina's depacketizers and a byte cap on the access unit
in progress): the RTSP client's cameras and the RTSP server's publications
depacketize through the same `TrackDepacketizer` (D26).

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
  `probe_timeout` (8 s) from the subscribe call. A subscription that has no
  track at that deadline and a source still starting (none has failed) goes on
  waiting to `first_track_timeout` (28 s from the call), once: FFmpeg probing
  a camera that sends a picture a second takes 20 s, and go2rtc waits that out
  (D27). A source that fails an attempt is given up at once: a camera that
  refuses the connection is still answered at the probe deadline, one that does
  not answer at all when its connection attempt fails (15 s, 16 s measured on
  an unreachable camera) and at the latest at 28 s. Both are under the egress deadlines:
  WHEP's answer and RTSP's `DESCRIBE` wait 29 s, and openIVR gives a WHEP
  offer 30 s.
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
  last release stops it after `idle_grace`, and a source that fails with no
  demand left is not retried through that grace (the next demand starts it at
  once). A run that fails, ends, produces
  nothing for `connect_timeout` or nothing more for `stall_timeout` is
  restarted. A source that has shown its cadence is restarted sooner
  (`cadence.rs`, D34): the channel keeps the longest gap between two frames
  of a connection, across connections, and the watchdog cuts a source silent
  for four times that gap, at least `silence_floor` (10 s), at most its stall
  timeout, which also holds until 8 gaps spanning 10 s are seen
  (`SourceError::Silent`); a source that reads this server (`local()`) keeps
  its stall timeout, its silence being its input's. The delay before the next
  attempt is `retry.rs`'s: a source that dropped after a healthy run (one
  that delivered for `healthy_after`) is asked again every `quick_retry`
  (500 ms, jittered to 375 ms) for `quick_retry_for` (2 min) while each
  attempt fails in under `cheap_attempt` (2 s) and its driver's attempts are
  connections (`SourceDriver::cheap_attempts`: the RTSP and HTTP clients, not
  FFmpeg), so that a camera back from an outage is read within half a second
  of its return (R01, R02); a peer that answered and refused
  (`SourceError::Busy`: an RTSP status other than 401 and 404, an HTTP status
  other than 404) keeps the backoff. Otherwise: exponential backoff with ±25 % jitter, capped at
  `backoff_max` (5 s: a camera that comes back is retried within 5 s; 30 s
  cost 18 s of recording after a 60 s outage in the bench's R01); backoff
  resets after `healthy_after` of good media. Refused credentials have their
  own, slower one (`auth_backoff_initial` 30 s doubling to
  `auth_backoff_max` 10 min): a changed password retried every 5 s would lock
  the account of a camera that counts failed logins, for every client. A run
  its peer declined with a client error (`SourceError::Declined`: an HTTP or
  RTSP 4XX, read from FFmpeg's `Server returned 4...`) has a slower one too
  (`declined_backoff_initial` 5 s doubling to `declined_backoff_max` 60 s),
  but only while the source has delivered nothing since its demand began: the
  sound transcode of a webcam with no microphone is declined at every run for
  as long as a viewer asks for its sound, and at 5 s that was two FFmpeg runs
  per camera every 5 s. A source that delivered keeps the fast backoff. FFmpeg's
  context addresses (`[rtsp @ 000001f97d146600]`) are left out of its error
  lines, so two runs of the same failure read the same and the retry warning
  is said only when the error changes and at attempts 2, 4, 8...
- **Channel**: one producer, many consumers. Each consumer has a **queue of
  its own** (`queue.rs`, D15) that carries track lists and frames in order,
  the frames shared by `Arc`. It is bounded by bytes (each item's payload
  plus `ITEM_OVERHEAD`, so tiny frames are bounded too) and by the age of
  its oldest item, by the consumer's class (`ConsumerTag::class`):
  `Limits::live_queue` (8 MiB, 2 s: WebRTC viewers, stills, preloads, and the
  RTSP readers that say `queue=live`; `webrtc.queue_*` bounds all of them) or `Limits::recorder_queue` (32 MiB, 10 s: RTSP
  readers, a segmenter or a sound transcode; `rtsp.queue_*`). An RTSP reader is a
  recorder unless its URL says `?queue=live` (D18: a detector that shows
  what it reads, and the `ffmpeg:` driver's own read of a stream for a
  `#video=` transcode with no `#audio=`; the class is listed as `queue`); a viewer's `User-Agent` and a reader's are kept, cleaned
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
  openIVR then holds the video). A given-up route keeps its source's demand,
  so a sound that fails for a moment can still join; a route whose source was
  DECLINED (`SourceError::Declined`, a 4XX) before it resolved is let go
  instead, before the first list and after it (checked every second while a
  route is unresolved), as go2rtc tries such a source once per consumer:
  kept, the sound transcode of a webcam with no microphone ran again every
  few seconds for as long as anyone watched it. A decline is said once per
  source at info until the source delivers, then at debug.

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

**Cameras over TLS** (`rtsps://`, D20): retina reads from a TCP socket of its
own and has no TLS, so `third_party/retina` is retina 0.4.22 with one hook,
`SessionOptions::connector`: the connection a session reads from is made by the
caller. `crates/rtsp-client/src/tls.rs` is that caller for an `rtsps` source:
it connects (`crates/net`, which the webcams' `https` shares: every address of
the name in turn, the families alternating), runs the rustls handshake within
four fifths of the source's stall timeout, and hands retina the secured
stream. retina asks the same connector for the fresh connections of a
background TEARDOWN, so no plain socket is ever opened to a camera named
`rtsps`. A certificate is not checked unless the source says `#verify`.

**Reconnecting to a camera** (D25): a new session waits for the TEARDOWN of the previous one (retina's `SessionGroup`) for at most `MAX_STALE_WAIT` (1 s). A source whose camera never answered TEARDOWN within that bound waits `SILENT_STALE_WAIT` (100 ms) from then on, until a wait sees an answer or a connection fails after a shortened wait (`crates/rtsp-client/src/stale.rs`).

**Webcams** (`crates/http-client`, D19) enter as Motion JPEG: a driver per
`http(s)://` source reads `multipart/x-mixed-replace` (or one JPEG per
request) and publishes each picture as a keyframe of a `Codec::Jpeg` video
track at 90 kHz, never decoded. A viewer cannot play it (no WebRTC codec
list names JPEG), so the stream a viewer opens lists an `ffmpeg:` transcode
after it, which reads the webcam back over the RTSP server like any reader:
the RTSP egress packetizes the JPEG as RTP/JPEG (RFC 2435: `jpeg.rs` reads
the markers of each picture for its size, chroma layout, quantization tables
and restart interval and cuts the scan at the payload budget; the tables go
in the first packet of every picture; a scan coded with Huffman tables of its
own is coded again with the standard ones by `recode.rs` on `huffman.rs`'s
tables, lossless and without decoding the picture). A picture RFC 2435 cannot describe
is dropped at that point, not earlier: `frame.jpeg` and the other consumers
still see it. An `https` connection that ends with no TLS `close_notify` ends the
data, as in Go (`eof.rs`): small cameras end a body with no length by closing
their socket.

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
- **A relayed candidate** per `turn:` URL: one allocation each, shared by
  every session, on a UDP socket of its own or, for `?transport=tcp`, on one
  TCP connection of its own (the relayed side is UDP either way; D13, D21).
  The TURN client is Sans-I/O (`turn/allocation.rs`, as str0m is); a task per
  server drives it (`turn/driver.rs`) over a link that is a socket or a
  connection (`turn/link.rs`: over TCP the stream is cut into messages by
  `stun::Framer`, TURN's framing with no length in front, a message beyond
  16 KiB read past not held), re-allocating
  after a failure with the core's jittered backoff (`turn/retry.rs`). A peer's data from the relay enters
  the same dispatch as the shared socket's,
  marked as arrived on the relayed address; a session's datagram leaves by
  the route of its source candidate (`socket.rs`: a TCP candidate's
  connection, a relayed candidate's TURN server, else the shared socket).
  Permissions for the IPs an offer lists are asked for while it is
  answered, so the viewer's first checks pass the relay.
- **The servers can be replaced while the server runs** (`PUT /api/webrtc/ice`,
  `WebRtcServer::set_ice_servers`, D22), because a TURN credential rotates and
  restarting the process would cut every recording and every viewer. A relay
  (`turn/driver.rs`) is one allocation made with one credential and never
  changes it; a new credential is a new relay beside the old one
  (`turn/servers.rs`, `IceServers::replace`). The new one, once it holds an
  allocation, retires the ones it replaces: they are no longer offered to a
  new session nor given permissions, but stay in the table the sessions'
  datagrams are routed by (`relay_for`, by relayed address), so a session
  keeps the path it was offered. A janitor (`reap_loop` in `server.rs`) stops
  a retired relay, which releases its allocation, once its grace has passed
  and no session has a route through its relayed address
  (`Registry::uses_relay`). A STUN server is a probe with a stop of its own and
  is added or removed in the same call.

**The session resource** (`resource.rs`, R45, D23). Each answered offer is a
resource, `Location: /api/webrtc/<id>`, `<id>` being 128 random bits in hex
(`getrandom`) that the registry maps to the session (`Registry::by_resource`,
filled at `register`, emptied at `retire`). `PATCH` (`WebRtcServer::trickle`)
reads an `application/trickle-ice-sdpfrag` body (`parse_fragment`: its
`a=ice-ufrag`, at most 16 `a=candidate` lines) and hands the candidates to the
session's `Rtc` under its lock (`add_remote_candidate`, panics contained as for
datagrams), then asks the TURN relays for permissions for their addresses, as
for an offer's. A user name fragment other than the offer's is an ICE restart:
`501`. A session takes 64 trickled candidates in all. `DELETE`
(`WebRtcServer::close`) cancels the session's token and waits (2 s at most) for
its `ended` token, which `session::run` fires through `Shared::retire` as soon as
it has dropped its subscription: by then the consumer has left the stream's
listing, the session no longer counts against `max_sessions` and
`max_sessions_per_ip`, and its id answers `404`. The ICE-TCP connections of the
session stay until the closing messages are out (`unregister`). The API crate
knows none of this: its `WebRtcAnswerer` trait carries `answer`, `trickle` and
`close`, and its router the two routes (`crates/api/src/resource.rs`).

A session with host candidates only is ICE-lite; one with a reflexive or
relayed candidate is full ICE, controlled (the browser controls).

**One MediaStream per session** (`msid.rs`, W1, D30). A browser builds one
`MediaStream` per stream id of the remote description. str0m names every
media line of an answer on its own (ids drawn per line, or the ids of the
offer's line when it has some) and has no call to choose them for an answer,
so `one_stream` rewrites them in the answer's text before `POST /api/webrtc`
returns it: every `a=msid:` and every `a=ssrc:<ssrc> msid:` of a media line
(the picture's SSRC and the SSRC of its retransmissions alike) name the
session's stream, 128 random bits in hex drawn per session, and a track
`<stream>-<n>`, `n` being the line's position; `a=msid-semantic: WMS` lists
that stream alone. Every other line is kept byte for byte, a refused line
names no stream, and no id an offer sent comes back as the server's. One
stream is also what a browser synchronises: it holds the picture for the
sound, 60 ms with a regular sound and 0.6 s on a real camera whose sound
stalls before W1b; since, about 40 ms of sound in L01 and 0.46 s on that
camera, and none with `webrtc.lip_sync: false` (below; D30 has the
measurements). The
rewrite reads text str0m serialised from a remote offer, so it has a fuzz
target (`answer_streams`) with its proptest twin.

**The sound of a session starts at its connection** (`session.rs`
`Sender::stale`, W1, D30). The subscription is attached when the offer is
answered and read once ICE and DTLS are up. The picture that waited is sent
(nothing decodes without its keyframe); the sound that entered oxrtc before
the connection is not, and is counted (`stale_sound` in the line a session
ends with). Sent in one burst it started the browser's audio buffer as deep as
the connection was long (164 ms in Edge on loopback), the buffer came back
down only to the top of the band it tolerates (about 90 ms, against about
45 ms for a stream that starts empty), and since the picture and the sound
are one stream the browser holds the picture for the sound: that depth was
the picture's latency.

**The sound's clock, and the bound** (`pace.rs`, `session_sound.rs`, W1b,
D30). The pacing described here is off by default since the review of W1b
(`Playout::pace_sound`): it moved the sound's wait from the browser to the
server without shortening it and cost 2.7 times the CPU of a viewer; the
clock and the bound below work without it, an on-time frame then being
written as it came. A camera's sound does not arrive as it was sampled: FFmpeg and most
cameras deliver 20 ms frames in pairs every 40 ms, and a camera on a poor link
stops for a second and then delivers that second at once. A browser's audio
buffer follows the worst lateness it sees, and since the picture and the sound
are one stream it holds the picture as long. Per sound track a session sends,
`SoundClock` learns the stream's clock from the frames (the earliest arrival
of the last 2 s against the frames' own times) and gives each frame the
instant it leaves: held, 40 ms at most (`MAX_HOLD`), so that the pairs leave
20 ms apart; a frame later than the hold leaves at once; a frame further behind
the clock than `webrtc.max_sound_late_ms` (400 ms by default) is not sent and
is counted (`late_sound` in the session's last line), unless the source is
that late in nearly every second (the lowest fifth of its last 30 s: a source
that delivers a second at a time is not cut). The frames wait in a queue of
64 at most (`Held`), the session's loop wakes for the next one, and a frame is
dated in the sender reports when it leaves, so the browser's lip sync sees the
sound as it is sent. The timer's own lateness is logged (`pace_overrun_ms`),
and the hold twice: the longest (`sound_hold_ms`) and the mean over every frame
(`sound_hold_mean_ms`), which is what the sound's path costs - in lip sync the
picture's latency too. The hold covers the worst per-second lateness of the
last 30 s, so on the bench's camera it sits at its bound: 38 ms on average in
L01, where the pairs alone ask 20.
The picture is not paced.

**Lip sync off** (`webrtc.lip_sync: false`, `negotiate.rs` `with_playout`,
W1b, D30). The answer then accepts the `playout-delay` header extension the
browser offers on its video line (Chromium does; an offer without it gets
none), and every picture carries a delay of 0 to 0: the browser shows each
picture as soon as it is decoded instead of holding it for the sound. The
answer still names one stream.

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

A request is not always answered by a replay (W3, D35). A browser that
decodes more slowly than the camera sends (fifty viewers in one browser on
the bench, a weak machine) drops frames and asks with nothing lost on the
wire; a group written to it within one frame interval is a burst it drops
too, so it asks again (B06: 1.5 times the camera's frames received, twice
go2rtc's freezes). `Replays::answer` leaves the request to the camera - the
picture stops until the camera's next keyframe, since nothing sent before
it can be decoded - when that keyframe is due within
`CAMERA_KEYFRAME_SOON` (1 s; `KeyClock`, per video track of the session,
takes the step between the last two keyframes it saw as the next one's),
or when the viewer asks again within `BEHIND_AFTER_REPLAY` (2 s) of a
replay. A group whose interval is not known yet (fewer than two keyframes
seen) or long (the owner's cameras: 4.8 to 6 s) is replayed as before. The
session's last line counts `keyframe_requests`, `replays`,
`keyframe_waits` (left to the camera) and `replayed_frames`
(`session_keyframes.rs`).

## Processes

One process. FFmpeg runs as child processes only when a transcode or a still
is needed, with an argument vector (never a shell), a cleared environment,
`kill_on_drop`, and its stderr drained into the log. A transcoder publishes its
output back into the local RTSP server on a one-time path token.

**Reading the relay with `rtsp.username` set (R28, D24).** The server asks every
reader for the operator's credential (`crates/rtsp-server/src/auth.rs`:
Digest or Basic, a nonce per connection, loopback included unless
`rtsp.local_auth: false`). An FFmpeg that reads the relay (an `ffmpeg:` source
naming a stream, or the relay's own URL) must not carry that password in its
argument vector, so `Publishers::register` issues each token a read credential
of its own, which `PublishHandle::read_credential` returns and the driver puts
in the input URL (`Plan::args_reading_as`) for that run only. It is live while
the handle is, and the publication itself needs none: the token is its
credential. The core, `oxrtc-ffmpeg` and the RTSP server meet only through
`PublishTarget`/`PublishLease`, as before.

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

The API closes a connection in two steps (D31, `crates/api/src/linger.rs`).
hyper answers as soon as a route has its answer and drops the connection when
the request's body was not read; closing the socket there resets a client
that is still writing, and a reset can destroy the answer it has not read.
So the dropped connection hands its socket to a task that ends the write side
and reads what the peer still sends, for at most `LINGER_TIMEOUT` (2 s) and
`LINGER_BYTES` (256 KiB), until the peer closes or the server stops. The
connection keeps its two slots (the total's and its address's) until the
socket is closed: a lingering connection is still counted.

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
`release.yml` runs it per platform (D17), and is what publishes a release, on
its tag; the script publishes nothing, and a tag that already has a release
(the fallback: built here because Actions could not run) is left alone by
the workflow, so the two never both publish (D33). The script first fetches
every crate of the lock file: the notices are made offline, from the whole
graph, the other platforms' crates included. A Linux binary built elsewhere is
checked by reading its ELF headers (`scripts/release_elf.py`), since it
cannot run there: glibc 2.35 is the floor, every loadable segment is aligned
to 64 KiB on arm64 (the Raspberry Pi 5's kernel uses 16 KiB pages), and no library outside glibc is
needed - aws-lc, the only C code (R15), is linked in. The Windows binary links
the C runtime in (`+crt-static`) and its PE imports are read the same way
(`scripts/release_pe.py`): Windows' own DLLs only, no `VCRUNTIME140.dll`.
