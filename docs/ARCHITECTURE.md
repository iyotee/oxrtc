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

`crates/audio` (`oxrtc-audio`, W5, D37) is a third helper, with no I/O and no
dependency on the core: G.711 A-law and mu-law (`g711.rs`, checked against
FFmpeg for every code and every sample), L16 (`pcm.rs`), a windowed-sinc
polyphase resampler between 8, 16, 32, 44.1 and 48 kHz (`resample.rs`: 16
zeros a side, Kaiser beta 8.6, aligned on its input; 86 to 88 dB from the
ideal on two tones, as FFmpeg's `aresample`), the G.722 encoder
(`g722.rs`, bit for bit FFmpeg's), a bounded WAV stream reader (`wav.rs`,
fuzzed: `fuzz/fuzz_targets/wav_stream.rs`), libopus' encoder through the
`opus` crate (`opus.rs`, feature `opus`, on by default) and `transcode.rs`,
which chains them: frames of one sound in, 20 ms packets of another out,
dated on the input's clock.

`crates/mp4` (`oxrtc-mp4`, M5 F1, D38) is a fourth: a fragmented MP4 writer
with no I/O, which depends on the core (its `TrackInfo` and `Frame` in, bytes
out) and on `oxrtc-audio` (the G.711 and L16 decoders it packs the PCM family
with). MSE over `/api/ws` and MP4 over HTTP use it (F3, D40; F4, D42: `oxrtc-api`
depends on it, still on neither WebRTC nor FFmpeg), through one subscription
and muxer of the API's (`crates/api/src/fmp4.rs`, below); HLS, DASH and
recording (F6, F19) are to be written with it ("Fragmented MP4" below).

`crates/mpegts` (`oxrtc-mpegts`, M5 F6) is a fifth: an MPEG transport stream
writer with no I/O, on the core alone (its `TrackInfo` and `Frame` in, 188-byte
packets out), for HLS's MPEG-TS segments and later `stream.ts` (F14, which
adds the sources and makes its reader public). The `AudioSpecificConfig` both
writers read is parsed once, in the core (`crates/core/src/aac.rs`: the MP4
writer's `esds` and codec string, the TS writer's ADTS headers).

## The core (`crates/core`)

- **Hub**: named streams. `upsert` with identical sources is a no-op (viewers
  keep watching - go2rtc tears the stream down and cuts them); a different
  source list closes the old stream and its subscriptions end with `Closed`.
- **Stream**: an ordered list of **slots**, one per source. A consumer's `Want`
  (video and/or audio, codec lists) is resolved per kind: the first slot that
  offers an accepted codec wins. An egress that reads which codecs its client
  takes (a WebRTC offer, an MSE codec list) makes its `Want` with
  `Want::preferred`, in the one order every egress shares
  (`oxrtc_core::PREFERENCE`: H.265 before H.264; Opus, AAC, then the PCM family). Offers come from the running connection's
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
  last release stops it after `idle_grace` (5 s, `api.idle_grace_ms`), or
  `transcode_idle_grace` for a driver whose runs transcode
  (`SourceDriver::transcodes`: an FFmpeg that encodes, not one that only
  copies a camera; 0, `api.transcode_idle_grace_ms`, D36), and a source that fails with no
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
  per camera every 5 s. A source that delivered keeps the fast backoff. A
  source whose last attempt was declined and that is waiting out that backoff
  is passed over by a new consumer (`Slot::declined_and_waiting`): it would
  not be asked before the backoff ends, and RC2's webcam viewers waited up to
  the 8 s probe deadline for it (D36); a source that is idle again is tried
  once. FFmpeg's
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

- **Parameter sets** (`bits.rs`, `h265.rs`, W7): the bit reader of a
  parameter set's RBSP (emulation prevention removed, every read checked), and
  an H.265 SPS's general profile, tier and level (`sps_profile_tier_level`),
  read once for the MP4 sample entries (`oxrtc-mp4`) and for the WebRTC
  answer's `profile-id`, `tier-flag` and `level-id` (`oxrtc-webrtc`); the
  `mp4_writer` fuzz target checks the two readings agree.
- **The output memory budget** (`memory.rs`, `host_memory.rs`, D45): one
  `MemoryBudget` on the hub that every queue of an HTTP or WebSocket reader
  draws from (`ConsumerTag::pooled`), sized from what the host gives the
  process (the cgroup - the tightest of its own group and its parents, a group's
  use read as its working set, its inactive page cache left out - `/proc/meminfo`,
  or the application's reading of the physical memory), read again every 5 s
  by the application. A pooled queue's bound is the smaller of its class's and its share, never below its stream's
  last group of pictures (`Channel` counts them; a stream with no picture has
  none, so no floor); a push that would overfill
  the budget first makes the reader furthest behind skip to its next keyframe
  (`make_room`, no queue's lock held: the registry, then one queue at a time).
- **What an H.265 receiver decodes** (`h265_receiver.rs`, `h265_fallback.rs`,
  D44): a WebRTC payload type's or an MSE codec string's profile space,
  profile, tier and level (`HevcReceiver`, the annex E string read by
  `from_codec_string`, fuzzed as `hevc_codec_string`), the one comparison
  with a camera's SPS (`takes`), and the one subscription that passes a
  camera's H.265 over for the stream's H.264 when its reader does not take it
  (`subscribe`), which the WebRTC egress and MSE both use.
- **JPEG pictures** (`jpeg.rs`, M5 F5, D46): what every Motion JPEG egress reads
  from a picture's markers (ITU-T T.81 annex B) - its size and whether it
  carries Huffman tables (`header`), the standard tables of annex K.3 written
  into one that has none (`with_huffman_tables`; the RTSP server's `huffman.rs`
  reads the same tables from here), whether a still is encoded again before it
  is served as a file (`wants_encoding_again`, go2rtc's `FixJPEG` rule), the
  shape a still is asked in (`Shape`, `Rotate`), and `Splitter`, which cuts
  FFmpeg's `-f mjpeg` byte stream into its pictures whatever the reads, a broken
  picture dropped and the next read, nothing held past its limit (fuzzed as
  `jpeg_split`, whose checks are `jpeg_invariants.rs`).
- **Derived streams** (`derived.rs`, `Hub::derived`, D46): a stream made from a
  stream by a source of its own, kept with it under a key (`mjpeg`:
  `ffmpeg:<stream>#video=mjpeg`, the Motion JPEG transcode of a stream with no
  JPEG source), the same one for every caller so that its readers share one
  source; an ordinary slot run on demand (started by its first reader, stopped
  after its idle grace by the last), out of the hub's list, closed with its
  stream (a replaced stream ends its derived readers), its source and readers
  shown in the stream's listing; four keys a stream at most.

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

**Reconnecting to a camera** (D25, amended 2026-11-24): a new session waits for the TEARDOWN of the previous one (retina's `SessionGroup`): `SILENT_STALE_WAIT` (100 ms) while nothing is known of the camera, `MAX_STALE_WAIT` (1 s) once it has answered within a wait, and not at all once it has not (silent); a connection that fails after a wait shorter than the full one takes the full wait back (`crates/rtsp-client/src/stale.rs`).

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
tables (`oxrtc_core::jpeg`'s annex K tables), lossless and without decoding the picture). A picture RFC 2435 cannot describe
is dropped at that point, not earlier: `frame.jpeg` and the other consumers
still see it. A JPEG picture carries no frame rate, and few cameras write
one in their H.264 or H.265 parameter sets: the `DESCRIBE` of a video track
says its rate (`a=framerate`, from the channel's `PictureRate`,
`crates/core/src/picture_rate.rs`: the newest 15 gaps between the dates of
the video track's pictures - their timestamps in the track's own ticks, not
their arrival, so a burst is not a pace - told after three, so that one
pause is not one either). The grid the dates sit on comes first, whatever
the codec: the coarsest step every date is a whole number of from the
first, within a tenth of it, judged as FFmpeg's own guess judges it - a
camera that stamps 25 slots a second and fills one in three is said 25, a
webcam of 30 a second whose pictures come evenly is said 30 (the whole
number of pictures a second when the dates fit one), which is what that
guess read; dates only near their slots are believed from a full window of
fifteen gaps. With no grid, a coded track says its median gap, and a Motion
JPEG track dated as it arrives its rate, however fast (`rate_interval`: the
mean of the gaps that are no pause, at most four times the mean of the
others, said as a whole number of pictures a second within 2.5 % of one, and
nothing above 120 a second, pictures that arrived together; the owner, 2026-11-20,
D36: a 30-a-second webcam on an uneven minute, which FFmpeg's guess reads
nothing of, is said 30 where its median read 26 to 29). The relay's `ffmpeg:`
transcodes of a stream read by name read it with `-fpsprobesize 0` when
FFmpeg is 8.0 or later (the first to read `a=framerate`; `version.rs` asks
`ffmpeg -version` once): FFmpeg's own guess read 20 pictures (20 s on a
webcam of one a second). Such a reader says what it is (`source=ffmpeg:` in
its query), and its `DESCRIBE` is held until the pace is known, 4 s at most
and never past its FFmpeg's own timeout less a second, the source's start
counted (`pace.rs`; the timeout is the source's `#timeout=`, else the
`ffmpeg.timeout` template, which the binary hands the server as
`RtspServerConfig::transcode_timeout`): its FFmpeg encodes at the rate that answer says, and with none
said x264 is told 90000 pictures a second - half the bits on the owner's
H.265 cameras (D36). An `https` connection that ends with no TLS `close_notify` ends the
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
`close`, and its router the two routes (`crates/api/src/resource.rs`);
`/api/ws` signals the same sessions over a socket ("go2rtc's WebSocket API"
below).

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

**The sound dated after its picture** (`webrtc.sound_lag_ms`, 20 ms by
default, `session_sound.rs` `picture_lead` and `dated`, W1c, D30). A browser
synchronises a session's two tracks from their RTCP sender reports - each
says "this RTP time is this wall clock" - and holds the picture until the
sound of the same date plays. With lip sync on, a session that sends a sound
dates its sound that many milliseconds after the picture that arrived with
it: the browser plays the sound that far behind the picture and shows the
picture it was holding that much sooner. Only the difference between the two
datings exists for the browser, and it is written as the picture dated
earlier than it arrived, the sound when it arrived: str0m takes no date in
the future (its sender report would carry RTP time 0 until the clock passed
it). `Sender::write` is the one place a frame is handed to str0m, so the
live frames, a replayed group of pictures and a paced sound are dated alike.
Nothing is moved with lip sync off, for a session that sends no sound, or at
`0`; 40 ms is the most a session is given. The browser adds its own share:
it stops moving the picture once it is within 30 ms of its target, which
leaves the sound about 10 ms behind the picture whatever the server does
(Edge, L01), and the lag adds to that: on the mean of a session's runs, by
the beep heard against the flash seen, at 20 ms the sound is about 23 to 31
ms behind its picture, at 25 about 44, at 30 about 62; a single run reads up
to 30 ms from its session's mean (D30 decision 6 and its review). The same
holds in openIVR's layout, where the sound a viewer gets is the camera's
transcoded to Opus by FFmpeg: the transcode does not put it further behind
(W1c review, 2026-11-20).

**Lip sync off** (`webrtc.lip_sync: false`, `negotiate.rs` `with_playout`,
W1b, D30). The answer then accepts the `playout-delay` header extension the
browser offers on its video line (Chromium does; an offer without it gets
none), and every picture carries a delay of 0 to 0: the browser shows each
picture as soon as it is decoded instead of holding it for the sound. The
answer still names one stream.

**H.265 on the camera's profile, tier and level** (`hevc.rs`, W7; D41, D44). The
offer's codecs choose the source (`OfferedCodecs::want`: H.265 before H.264),
and `parse_offer` keeps what each H.265 payload type receives as an
`oxrtc_core::h265_receiver::HevcReceiver` (32 types at most): str0m's reading
of its `profile-id`, `tier-flag` and `level-id` when all three are given, else
its profile (Main without one) and the `level-id` the line writes, if any
(`written_level_id`): an absent level is no ceiling. The rule and the
subscription are the core's, which MSE calls too: `h265_fallback::subscribe`
reads the camera's SPS once the subscription has its tracks, and when no
receiver takes it (`HevcReceiver::takes`: the same profile space; its
profile, or one the camera's compatibility flags name, or any Main picture
for a Main 10 receiver (H.265 A.3.3); a tier no higher; a
level no higher than one named) the same wish without H.265 is subscribed
to, which picks the stream's next source (openIVR's H.264 transcode), the
first subscription held until that one has tracks so the camera is not let
go in between; a stream with no such source sends the camera's H.265
anyway. `negotiate.rs` tells str0m the
camera's own numbers (`add_h265`) in place of `enable_h265`'s Main, level
6.0, so str0m matches the offer's payload type of that profile (Main 10 is
not Main) and the answer names the camera's level (str0m answers the lower
of the two); a camera sent though no payload type takes it, or whose SPS is
not known yet, is told as the offer's first H.265 payload type, which str0m
matches (`hevc::answered`). Chrome 136+ offers Main and Main 10 at level 6.0 when the
machine decodes H.265 (Edge 157 offers none); the owner's XM cameras are
Main, level 5.0.

**An offer that shares no codec with the stream** (`mismatch.rs`, D43). One
refusal for `POST /api/webrtc` and `/api/ws`, which both answer through
`WebRtcServer::answer_resource`: `WhepError::NoMatchingCodec` carries go2rtc's
words, the stream's codecs then the offer's (`streams: codecs not matched:
video:H264 => video:VP8`), the offer's read by `parse_offer` from every
receiving line (`OfferedCodecs::named`). An offer whose video line lists no
codec this server sends subscribes for the stream's picture in any codec
(`OfferedCodecs::answer_want`): a picture comes, the offer is refused before a
session is made; none (a stream of sound alone), the sound is answered and
`negotiate.rs` answers the video line on the offer's own VP8, VP9 or AV1,
nothing sent on it, so that a client that refuses a rejected line (aiortc)
still takes the answer.

**A session its viewer never reached** (`unreached.rs`, W7). `Peer::heard`
says whether any datagram or TCP frame of the peer reached the session. A
session that never connected ends `no packet from the viewer reached this
server` (or `ice never connected` when packets came) where str0m's `ice
disconnected` or the idle timeout said a connection was lost that never
existed, and a warning names the likely causes (a firewall, the answer's
candidates), at most once a minute for the process.

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
seen) or long (the owner's cameras: 4.8 to 6 s) is replayed as before, and
so is one whose expected keyframe is late by more than a quarter of its
step (`KEYFRAME_LATE`: a source that restarted mid-group or an encoder's
keyframe at a change of scene made the step short, and the next keyframe
may be a whole group away). The
session's last line counts `keyframe_requests`, `replays`,
`keyframe_waits` (left to the camera) and `replayed_frames`
(`session_keyframes.rs`).

## go2rtc's WebSocket API (`crates/api/src/ws`)

`GET /api/ws?src=<stream>` (M5 F2, D39) signals the WHEP egress's sessions
over a WebSocket, as go2rtc does for Home Assistant, Frigate's cards and its
own player, and serves MSE over it (F3, D40): fragmented MP4 from
`oxrtc-mp4`, written by the socket itself from a subscription of its own. axum's own upgrade (its `ws` feature, over tokio-tungstenite) is
the only WebSocket code; the API crate still knows nothing of WebRTC: an
offer goes through `WebRtcAnswerer::answer`, a candidate through `trickle`
(as one `a=` line of a fragment), the session of an answer nobody could
receive through `close`.

| File | What |
|---|---|
| `mod.rs` | the route: the origin check (`OriginPolicy`, `api.origin`), the upgrade with its message, frame and write-buffer caps, the socket's two slots (`crate::slots::Slots`: the total and its address's), the listener told the connection now carries frames; `WsLimits` |
| `message.rs` | the parser of a text frame (`Incoming`: an offer in either dialect, a candidate, `mse`, `mjpeg`, a type of a later item, anything else ignored) and the messages written (`Outgoing`, the error's reason redacted); fuzzed (`fuzzing.rs`, target `ws_message`) |
| `conn.rs` | one task per socket: its messages in order, the offer answered beside them (a cold source takes seconds while candidates arrive), candidates held until the answer, the socket's media (`media.rs`) written as it comes, the rate bound, pings and the idle bound, the server's stop; `hls` (an fMP4 HLS session opened by `crate::hls::open` in the list's codecs, answered with its master playlist, counted with the offers); `later_error`, the one place F4 plugs `mp4` in |
| `media.rs` | what a socket streams beside its offers, one at a time and counted with them: an `mse`'s MIME type, init segment and fragments (`crate::fmp4`'s `Feed`), or an `mjpeg`'s answer then one JPEG per binary frame (`crate::mjpeg`'s `Pictures`, opened beside the loop, cancel-safe); an error before the player was told what follows leaves the socket, one after closes it |
| `mse.rs` | `mse`: the codec list to a `Want` and an H.265 entry (`wish`, the family of each name, then `Want::preferred`, the order WebRTC's offer uses too); the subscription and its muxer are `crate::fmp4`'s `Feed` (below) |

A session outlives its socket, as go2rtc's does (its player closes the
socket once WebRTC plays): the socket holds only the resource id of its last
answer, to route later candidates. An MSE does not: its subscription lives
beside the socket's loop (like the offer in flight, a branch polls it while
another uses the socket) and is dropped with it. Its queue is the core's
queue for that consumer: the socket writes a fragment before it reads the
next event, and its writer holds one message, so a slow reader backs up the
core's live budget, which drops and resumes at a keyframe (D40). The listener's `Progress` carries an
`upgraded` flag that `mod.rs` sets before the `101` is written, so that the
request-head clock (`HEAD_TIMEOUT`) no longer starts on the connection's
reads; its idle timeout still runs, and the socket's pings keep it from
firing. A `Shutdown` extension that `serve_with_limits` adds to every request
carries the server's stop to each socket, which the listener's graceful stop
does not wait for.

## MP4 over HTTP, and the feed MSE shares (`crates/api/src/{fmp4,mp4}.rs`)

`GET /api/stream.mp4` and `GET /api/frame.mp4` (M5 F4, D42) and MSE over
`/api/ws` (F3) write fragmented MP4 through one path: a `Feed`
(`fmp4.rs`), a subscription and its muxer, whose events become the MIME type
once, an init segment, then one fragment per frame. Each egress only
carries what it returns: WebSocket messages, or an HTTP body.

| File | What |
|---|---|
| `fmp4.rs` | `Feed` (subscribing, then the subscription's events through the muxer; cancel-safe for a socket's loop or a body's), `Wish`, `Step`; losses logged once per 10 s, named by the egress (`mse`, `mp4`) |
| `fmp4/container.rs` | what a muxer writes into, behind its one track choice: `Mp4` (`oxrtc-mp4`, one fragment per frame) and `Ts` (`oxrtc-mpegts`, one PES per frame, the program's tables when a segment asks), each saying which tracks it takes (`supported`), built once the tracks' configuration is there, giving its MIME type and codec strings |
| `fmp4/muxer.rs` | (`MuxerOf<C>`, generic over the container) the events as an fMP4 reader takes them: the first video and audio track the wish takes, the MIME type once, `Init` per new track list of the same codecs, one fragment per frame from a keyframe (at the start, after a loss, after a refused picture), times moved so the reader's media starts at zero, the latest frame's time (what `duration` reads) |
| `mp4.rs` | the two routes: the query to a `Wish` as go2rtc reads it (`stream_wish`: `mp4`, `mp4=flac`, `mp4=all`, the `video`/`audio` filters, the default), `duration`, `filename` to `Content-Disposition`, the head (the feed until its first fragment, with the MIME type and init segment it is written under: tracks that come again before it, a camera's parameter sets in band other than its description's, replace both) then a body that ends on `duration` (media, then wall clock), a new init segment, 75 s of silence, the server's stop or its client's going; `frame.mp4`'s init segment and first fragment with their length; `Mp4Limits`; fuzzed (`fuzzing.rs`, target `mp4_query`) |
| `slots.rs` | `Slots`: long-lived answers counted in all and per address (`/api/ws`'s sockets, MP4 responses, `stream.mjpeg`'s beside them under the same bound), refusals logged once per interval |

A body is a stream of chunks (`futures_util::stream::unfold`) that owns the
feed and the response's slot: hyper drops it when a write to the client
fails or the server stops, and the subscription ends with it. hyper pulls the
body only as fast as the client reads, so a slow reader backs up the core's
queue for that consumer, as for MSE: no buffer of the response's own grows.

**Where a reader starts** (D42 as amended, M5 D (5)). The core's consumer
carries a `Start` (`ConsumerTag::start`): `Cached` (every egress's default)
attaches each first route with the source's cached group of pictures, so
the first picture is that group's keyframe; `NextKeyframe` (`start=next` on
`stream.mp4`, `frame.mp4`, and on `/api/ws`'s URL for its `mse`) attaches
without it (`Route::attach`), and the route's own wait for a keyframe then
starts the reader at the first one after the request, as go2rtc starts
every consumer. A source change later resumes from the new source's cache,
whatever the start.

## Motion JPEG and stills (`crates/api/src/{mjpeg,snapshot}.rs`)

`GET /api/stream.mjpeg` and the `mjpeg` message of `/api/ws` (M5 F5, D46) read
their pictures through one `Pictures` (`mjpeg.rs`): a pooled subscription that
wants Motion JPEG alone, on the stream itself when one of its sources offers it
(an MJPEG camera, a configured `ffmpeg:<stream>#video=mjpeg`: its pictures
passed on, never decoded), else on its derived stream `ffmpeg:<stream>#video=mjpeg`
(`Hub::derived`, above: one FFmpeg for every reader of the stream), which the
`ffmpeg:` driver runs with its pictures on FFmpeg's standard output
(`crates/ffmpeg/src/pipe.rs`, cut by `oxrtc_core::jpeg::Splitter`), not
published back over RTSP: RTP/JPEG cannot carry sides over 2040 pixels nor
the tables FFmpeg writes. Each picture goes out with the standard Huffman
tables written in when it has none.

| File | What |
|---|---|
| `mjpeg.rs` | `Pictures` (the stream's own JPEG, else its transcode; losses logged once per 10 s); `stream_mjpeg`: the slot (shared with the MP4 responses), the head once the first picture is there (or the server's stop), then a body of parts (`part`: go2rtc's part headers and `Content-Length`, the next delimiter after each picture) ended by its client, the server's stop, the stream's end or 75 s without a picture |
| `snapshot.rs` | `frame.jpeg`: the query's shape (`width`/`w`, `height`/`h`, `rotate`) and `cache` (a Go duration, `go_duration`) read as go2rtc reads them, what cannot be read `400`; the picture made by the `Snapshotter` (the binary's FFmpeg stills, `crates/ffmpeg/src/stills.rs`: scaled then turned, a JPEG picture asked as it is passed on), kept for `cache` per stream and shape (`Stills`: 64 pictures, 32 MiB) |

## HLS and DASH (`crates/api/src/hls/`)

`GET /api/stream.m3u8` and `GET /api/stream.mpd` (M5 F6, D47) open a session
of the client's own: a `FeedOf<Mp4>` or `FeedOf<Ts>` (the feed and muxer MSE
and `stream.mp4` use, with the same track choice and H.265 rule), driven by a
task of the session that writes each frame into its window; the session's
routes under `/api/hls/` read the window, waiting on it (a `watch` of its
changes) for a blocking reload or a part asked early.

| File | What |
|---|---|
| `hls/mod.rs` | `Hls`: the sessions by id (22 characters of base 62 from 16 random bytes), 64 and 16 per address, the requests waiting (`Slots`, 512 and 32 per address); `HlsLimits` |
| `hls/routes.rs` | `stream.m3u8` (the query to fMP4 or MPEG-TS as go2rtc chooses, the session opened once its codecs are known, go2rtc's master playlist), `stream.mpd` (the manifest of the query's one DASH session, read again by every reload, its segments under `<BaseURL>hls/</BaseURL>`), the session's routes (`playlist.m3u8` with `_HLS_msn`/`_HLS_part`, `init.mp4`, `segment.m4s`/`.ts`, `part.m4s`/`.ts`, `manifest.mpd`), read with the id alone, `Access-Control-Allow-Origin: *` |
| `hls/session.rs` | `Session`: the window, the init segment, the codecs, the wall clock of its zero (the hub's epoch on the wall clock plus the first frame's time), `wait` for a condition under a bound, `run`: the feed into the window (the TS tables before a segment and before an independent part), the window evicted to the session's memory share (`Held`, D45), ended after its idle time (never while a request waits on it), its feed's failure or the server's stop |
| `hls/window.rs` | `Window`: segments cut at a keyframe once 1 s long, parts at each keyframe and at three quarters of 100 ms, only the lead track's times cutting; 7 segments, the memory share, never fewer than 2; the init segments its segments are read with (`init_segment`), the discontinuities that left it; what is complete (`has`, `last_part`, `next_part`) |
| `hls/playlist.rs` | the text: go2rtc's master playlist, the media playlist (`TARGETDURATION` the longest segment rounded up, `PROGRAM-DATE-TIME` per segment, `MAP` per init segment (each segment's own, `init.mp4?i=`), `DISCONTINUITY` after a new track list and `DISCONTINUITY-SEQUENCE` once one has left the window, Low-Latency's `SERVER-CONTROL`, `PART-INF`, `PART` and `PRELOAD-HINT`), the dynamic MPD with its segment timeline; `rfc3339` |

## The rest of go2rtc's API (`crates/api/src/control.rs`, M5 F7)

The application's own routes, as go2rtc names them. `GET /api` answers the
go2rtc API version this server implements (`GO2RTC_API_VERSION`, 1.9.15)
with oxrtc's release as its build metadata, and the release again as
`oxrtc_version` (D48), the request's `Host`, the configuration file's path
and the RTSP server's address (`AppFacts`, set by the binary through
`ApiState::with_config_path` and `with_rtsp_listen`; `apps/oxrtc`'s
`Launch` carries the path from the command line to `App::start_with`).
`GET /api/schemes` lists the hub's registered schemes (`Drivers::schemes`).

## Fragmented MP4 (`crates/mp4`)

A `Writer` is built from a subscription's track list and gives an init
segment (`ftyp`, `moov` with one `trak` per track and `mvex`/`trex`) and media
fragments (`moof`: `mfhd`, one `traf` per track with samples - `tfhd` with
the base at the `moof`, a 64-bit `tfdt`, `trun` with each sample's duration,
size, flags and, when a decode time differs, its signed composition offset -
then one `mdat`). A new track list (a source that reconnected with other
parameter sets) takes a new writer.

| File | What |
|---|---|
| `writer.rs` | `Writer`, `Fragmenting` (every frame, per group of pictures, per duration with a cut at each keyframe), `Fragment` (independent or not, its `Span` on the core's timeline) |
| `timing.rs` | decode times contiguous per track, durations that follow the source (D38) |
| `track.rs` | a track's sample entry, codec string and clock; a frame to a sample |
| `h264.rs`, `h265.rs` | the SPS fields the entries need, `avcC`, `hvcC`, `avc1.PPCCLL`, `hvc1.P.C.TL.B` |
| `aac.rs`, `opus.rs` | `esds` and the codec string from the `AudioSpecificConfig` the core reads (`oxrtc_core::aac`, shared with the MPEG-TS writer); a packet's duration from its table of contents and `dOps` |
| `flac.rs` | G.711 and L16 as FLAC frames of verbatim subframes, `dfLa` |
| `init.rs`, `fragment.rs`, `boxes.rs` | the boxes (the bit reader of the parameter sets and the H.265 profile, tier and level are the core's, `bits.rs` and `h265.rs`) |
| `reader.rs`, `reader_fragment.rs`, `fuzzing.rs` | tests and fuzz targets only: an independent reader of what was written, and the harnesses of `mp4_reader` and `mp4_writer` |

| Track | Sample entry | Codec string |
|---|---|---|
| H.264 | `avc1`, `avcC` (with the high profiles' chroma and depth) | `avc1.64001e` |
| H.265 | `hvc1` (or `hev1`), `hvcC` with the VPS, SPS and PPS arrays (complete for `hvc1`, not for `hev1`) | `hvc1.1.6.L120.90` |
| AAC | `mp4a`, `esds` | `mp4a.40.2` (the object type) |
| Opus (48 kHz, 1 or 2 channels) | `Opus`, `dOps` | `opus` |
| G.711 A-law, mu-law, L16 | `fLaC`, `dfLa` | `flac` |

Motion JPEG has no entry here. `supported` says what `Writer::new` takes, or
will once a missing configuration arrives (no Motion JPEG, no Opus beyond two
channels or off 48 kHz, no PCM beyond eight channels), so that an egress
keeping the tracks it says yes to never builds a writer that fails. Every input is a
camera's or a source's: the parameter sets are read with a bounds-checked bit
reader, a frame the box format cannot carry (length prefixes that do not add
up, an Opus packet with no valid table of contents, PCM that is not whole
samples, over 16 MiB, a composition offset beyond 32 bits) is refused with an
`Mp4Error` and changes nothing, and a fragment is closed before 4096 samples
or 32 MiB. Proven against a reader of its own (unit tests, two fuzz targets),
against FFmpeg (`tests/ffmpeg.rs`: ffprobe reads the codecs, one packet per
frame and the source's own times; ffmpeg decodes without an error; A-law as
FLAC decodes to FFmpeg's own A-law samples; B-frames keep their presentation
times) and in headless Edge's MSE (`tests/browser/mse.mjs`, by hand; the
crate's README).

## MPEG transport stream (`crates/mpegts`)

A `Writer` is built from a subscription's track list: one program, its PMT
on PID `0x1000` and a PID per track from `0x100` (FFmpeg's numbers).
`tables()` gives the PAT and the PMT, one packet each, what an HLS segment
starts with; `push` gives a frame's PES cut into packets, the presentation
time always, the decode time when it differs, on the 90 kHz clock moved 0.7 s
ahead (`TIME_OFFSET`, FFmpeg's `muxdelay`) so that the program clock
reference, carried on the first video track (else the first track) at each
of its PES as its decode time without that offset, never runs ahead of a
decode. Continuity counters run across calls, so a segment is the tables then
the bytes of its frames; times wrap at 33 bits, as every reader expects.

| File | What |
|---|---|
| `writer.rs` | `Writer` (`new`, `tables`, `push`, `push_decoded`, `pids`), `supported` |
| `es.rs` | a frame as its elementary stream: H.264 and H.265 in Annex B behind an access unit delimiter (unless the frame has one), the track's parameter sets before a keyframe that carries none; AAC behind an ADTS header from the core's `AudioSpecificConfig` reading; Opus behind its control header |
| `pes.rs` | the PES header, its times with their marker bits, its length (written for sound, 0 for video) |
| `packet.rs` | a PES cut into packets (the PCR and `random_access_indicator` in the first one's adaptation field, the last filled by stuffing), a table in one packet with its pointer field |
| `psi.rs`, `crc.rs` | the PAT and the PMT (an `Opus` registration and its extension descriptor for Opus), their CRC-32/MPEG-2 |
| `reader.rs`, `fuzzing.rs` | tests and fuzz targets only, for now: a reader of whole packets (sync, counters, the tables with their CRC, each PES with its times, PCR and random access), and the harnesses of `mpegts_reader` and `mpegts_writer`; F14 makes the reader the one of MPEG-TS sources |

| Track | Stream type | Elementary stream |
|---|---|---|
| H.264 | `0x1B` | Annex B, `stream_id` `0xE0` |
| H.265 | `0x24` | Annex B, `0xE0` |
| AAC (object types 1 to 4 of the core coder, a rate of the table, a layout by number) | `0x0F` | ADTS, `0xC0` |
| Opus (48 kHz, 1 or 2 channels) | `0x06`, registration `Opus` | the control header, `private_stream_1` |

G.711, L16 and Motion JPEG have no stream type here (go2rtc's TS writer takes
H.264, H.265 and AAC; MediaMTX's Opus too). A frame the format cannot carry
(length prefixes that do not add up, an empty sound frame, an AAC frame over
ADTS's 13-bit length, over 16 MiB, a negative time, a decode after its
presentation) is refused and writes nothing. Proven against the reader (unit
tests, two fuzz targets with their proptest twins) and against FFmpeg 8.0.1
(`tests/ffmpeg.rs`, on the MP4 writer's FFmpeg-encoded input,
`crates/mp4/tests/common`: ffprobe reads `h264`+`aac`, `hevc`+`opus`, AAC
alone, one packet per frame at its time with its keyframe flag; ffmpeg decodes
with `-xerror` and says nothing).

## Processes

One process. FFmpeg runs as child processes only when a transcode or a still
is needed, with an argument vector (never a shell), a cleared environment,
`kill_on_drop`, and its stderr drained into the log. A transcoder publishes its
output back into the local RTSP server on a one-time path token, except a
Motion JPEG one (`#video=mjpeg` alone, D46): its pictures come on FFmpeg's
standard output (`crates/ffmpeg/src/pipe.rs`, go2rtc's `output/mjpeg`), cut and
published by the driver itself, its token held only for the read credential of
the relay it reads.

**A sound transcode with no process** (W5, D37). An `ffmpeg:` source that
asks for one `#audio=` of Opus, G.711 or L16 from an input oxrtc reads
itself is decided native when the factory builds it
(`crates/ffmpeg/src/native/plan.rs`): a stream of this hub by name or by its
loopback RTSP URL, or an `http(s)` URL with no `#input=` but a template of
probing flags (openIVR's `dedup_input`). Its driver (`native/mod.rs`) wraps
FFmpeg's and answers everything the core asks as FFmpeg's would; its run
reads the input itself - a subscription of the hub, the sound alone, with a
recorder's queue (`native/stream.rs`, listed as the consumer
`native_audio`), or a `GET` through the webcams' HTTP client, which the
binary hands over as `AudioHttp` (`native/wav.rs`, `WavBodies` in
`apps/oxrtc/src/backends.rs`; no further than 10 s ahead of the wall clock, the
core's `MAX_LEAD_SECS`, so a server that sends faster than time waits on TCP) - and publishes what `oxrtc-audio`'s
`Transcoder` makes of it (`native/publish.rs`): the output's track, then a
packet every 20 ms, dated where its first sample was in the input and
received when that sample was (D12), never before the packet before it, and
published no sooner than that instant: the later packets of a camera's 64 ms
frame wait 20 or 40 ms, since
a WebRTC sender handed a wall clock in the future writes sender reports of
RTP time 0 (D37 "Corrected"). An input that is not G.711 or L16 (AAC,
Opus, a WAV of another coding, an answer that is not `audio/wav`) runs FFmpeg
in the same run, before anything is published, and an HTTP input found so is
FFmpeg's from the start of every later run (`NativeDriver::not_native`: the
server is opened once an attempt, not twice). `ffmpeg.native_audio: false`
builds FFmpeg's driver for every source.

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

On Windows FFmpeg is started with no console (`DETACHED_PROCESS`,
`crates/ffmpeg/src/console.rs`): `CREATE_NO_WINDOW` gave each one a console
of its own, a `conhost.exe` of 7 MiB beside it (56 MiB for openIVR's six
cameras, D36). A `bin` that is a bare name or a launcher's (Chocolatey's
`chocolateyin`, Scoop's `shims`) keeps `CREATE_NO_WINDOW`: a launcher
detached opens a visible console for the FFmpeg it starts.

x264 is given two threads (`-threads:v 2`, before the output) when an FFmpeg run's
arguments name `libx264` and none of its templates says a thread count
(`crates/ffmpeg/src/x264.rs`, D49): told nothing, x264 under `-tune zerolatency`
slices each picture over one and a half threads per logical CPU and spends a third to
two fifths of a transcode's CPU synchronising them (the owner's 1080p transcode: 38 to
43 % less CPU and 35 to 60 MiB less memory with two, on Windows and Linux alike). An
operator's `-threads` or `threads=` wins; no other encoder is touched.

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

A WebSocket of `/api/ws` is a connection of the API's bounds and has its own
on top (D39): 128 sockets and 64 per address, 64 KiB per message, 50
messages a second, a ping every 20 s and 60 s of silence at most; once
upgraded, its frames start no request-head clock. A response of
`stream.mp4` or `frame.mp4` holds its connection for as long as it lasts
too, and has its own bound on top (D42): 64 at once and 16 per address, the
head within 60 s, 75 s without a fragment written at most for `stream.mp4`,
30 s for `frame.mp4`'s keyframe. A `stream.mjpeg` is counted with them (D46),
under the same 75 s without a picture.

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
