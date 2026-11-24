# Compatibility with go2rtc

The surface openIVR uses, read from its source (`app/services/streaming/
go2rtc_manager.py`, `audio_dedup.py`, `app/services/recording/`,
`app/services/ai/frame_capture.py`) on 2026-11-14, against go2rtc 1.9.15. This
is the contract of the compatibility layer: everything here must behave as
described, and anything not here is out of scope until a consumer needs it.

## Process

- Launched as `oxrtc -config <path>` (go2rtc's flag; `--config` too).
- `-version` (or `--version`) prints go2rtc's line with this program's name,
  `oxrtc version 0.1.0 windows/amd64`: openIVR's installer reads the word after
  `version` (`install.py`, `version (\S+)`).
- Logs to **stdout**, one line per event, with go2rtc's three-letter levels
  (`TRC DBG INF WRN ERR`) as the second word: openIVR relays each line to its
  own log at that level.
- Healthy when `GET /api` answers 200, within 10 s of launch.
- Stops on SIGTERM (Unix) or `terminate()` (Windows) within 5 s.
- Its FFmpeg processes die with it, killed or crashed included; go2rtc's
  outlive a killed go2rtc. On Linux this needs **`oxrtc-exec` beside the
  `oxrtc` binary** (the release archives carry both; an installer that copies
  `oxrtc` alone must copy it too): without it, oxrtc warns at startup and
  runs FFmpeg unconfined, as go2rtc does. Through it FFmpeg cannot read
  outside its own files and the system's (ARCHITECTURE "Processes"), so an
  operator's template that reads or writes a file, or an FFmpeg installed as
  a snap (a set-user-ID launcher), does not run there.

## Configuration file (go2rtc YAML subset)

```yaml
api:
  listen: "127.0.0.1:1984"
  username: "..."        # optional in go2rtc; enforced by oxrtc on every call
  password: "..."
  origin: ""             # pages that may open /api/ws: own host (default), "*" any
rtsp:
  listen: "127.0.0.1:8554"
webrtc:
  listen: ":8555"
  candidates: ["192.168.1.42"]        # extra host candidates, with or without :port
  ice_servers:                        # STUN/TURN for viewers outside the LAN
    - urls: ["stun:relay.example:3478", "turn:relay.example:3478?transport=udp", "turn:relay.example:3478?transport=tcp"]
      username: "..."
      credential: "..."
streams:
  camera_1_src: ["rtsp://user:pass@192.168.1.100:554/live/ch00_0#timeout=15"]
  camera_1: [
    "rtsp://127.0.0.1:8554/camera_1_src?video#timeout=20",
    "ffmpeg:camera_1_src#video=h264#width=1920#height=1080",
    "rtsp://127.0.0.1:8554/camera_1_rec?audio=opus",
  ]
ffmpeg:
  bin: /path/to/ffmpeg
  h264: "-codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough"
  dedup_input: "-fflags nobuffer -flags low_delay -probesize 32 -i {input}"
log:
  level: info
```

Lists are written in YAML flow style by openIVR; both styles must parse. Unknown
keys are warned about and ignored. The file holds camera credentials: it is
read, never written back.

One go2rtc key about protection is not followed: `api.local_auth: false` is
warned about, because oxrtc authenticates loopback callers of the API too (D7);
`true`, which openIVR writes, is what oxrtc does anyway.

`rtsp.username` and `rtsp.password` **protect the RTSP server** (R28, D24),
both or neither (one without the other refuses the file, and so does a user
name holding `:` or a value over 256 bytes; empty is no protection, as in
go2rtc). Every reader then gives the pair, as Digest or as Basic (see "RTSP
server" below), **loopback readers included**: go2rtc 1.9.15 takes a peer on
loopback unchecked, even with a wrong password (measured), and oxrtc does not,
because on a machine that also runs a backend open to SSRF a loopback
unprotected RTSP server is the credential's whole point. `rtsp.local_auth:
false` (oxrtc only) gives go2rtc's rule back. openIVR writes none of these:
its RTSP server listens on loopback and its readers send no credential.

**oxrtc-only keys.** A key go2rtc does not have goes in the section of what it
configures (never in a section of its own), and leaving it out means oxrtc's
default: a file written for go2rtc runs unchanged, and go2rtc, which ignores
keys it does not know, still reads a file that carries them. Today:

| Key | Default | Meaning |
|---|---|---|
| `api.token` | none | bearer token, besides HTTP Basic (D7) |
| `api.allow_unauthenticated` | `false` | explicit, logged opt-in to an open API (D7) |
| `api.max_connections_per_ip` | 128 | API connections one address keeps open (D9) |
| `rtsp.max_connections_per_ip` | 128 | RTSP connections one address holds (D9) |
| `rtsp.local_auth` | `true` | whether a reader on loopback must give `rtsp.username`/`rtsp.password` too; `false` lets loopback in unchecked, as go2rtc does (D24) |
| `webrtc.max_sessions` | 64 | WHEP sessions at once, every caller together (D9) |
| `webrtc.max_sessions_per_ip` | 32 | WHEP sessions one caller address holds (D9) |
| `webrtc.lip_sync` | `true` | the browser holds the picture for the sound, as go2rtc and MediaMTX do; `false` sends the picture with the `playout-delay` header extension at 0, which Chromium-based browsers read as "show it as soon as it is decoded": the picture no longer waits for the sound, and the sound trails it by its own buffer (D30, W1b) |
| `webrtc.max_sound_late_ms` | 400 | sound further behind its stream's own clock than this is not sent to a viewer: what a stalled camera delivers at once when it comes back, which a browser would otherwise buffer, and hold the picture as long; `0` sends all of it. A source whose every second comes that late is not cut (D30, W1b) |
| `webrtc.sound_lag_ms` | 20 | with lip sync on, how many milliseconds after the picture that arrived with it the sound is dated (in the RTCP sender reports a browser synchronises from): the browser plays the sound that far behind the picture, and shows the picture it holds for the sound that much sooner. 0 to 40; `0` dates both when they arrived. A browser leaves the sound about 10 ms behind the picture by itself, and the lag adds to that (Edge, on the bench's camera, by the beep heard against the flash seen, on a session's mean): at 20 the sound is about 23 to 31 ms behind its picture, 7 to 14 more than MediaMTX's; at 25 about 44; at 30 about 62, past the 45 ms ATSC IS-191 gives a sound behind its picture. A single run reads up to 30 ms from its session's mean. Nothing with `lip_sync: false` (D30, W1c) |
| `ffmpeg.max_processes` | 64 | FFmpeg processes at once, stills included; go2rtc reads it as a template nothing names |
| `ffmpeg.max_memory_mib` | 4096 | address space of each FFmpeg (`RLIMIT_AS`), Linux only, where FFmpeg runs confined through `oxrtc-exec` (ARCHITECTURE "Processes"); go2rtc reads it as a template nothing names |
| `ffmpeg.native_audio` | `true` | an `ffmpeg:` sound transcode (`#audio=opus`, `pcma`, `pcmu`, `pcm` and their rates) of a stream of this server, its loopback RTSP URL or an `http(s)` WAV is made in process when the input is G.711 or L16, with no FFmpeg (W5, D37); `false` runs FFmpeg for every source, as before; go2rtc reads it as a template nothing names |
| `api.max_streams` | 512 | streams at once, from the file and `PUT /api/streams` together (D16) |
| `api.max_consumers_per_stream` | 64 | viewers, readers and preloads of one stream; one more is refused (`503` to an offer or a `DESCRIBE`) (D16) |
| `api.max_frame_mib` | 8 | largest frame a source may publish, in MiB; a larger one is refused and counted in the source's `rejected` (D16), and a camera's access unit cut at it while it is built is counted there too (D26) |
| `api.output_memory` | `"12.5%"` | (**oxrtc only**, D45) the memory budget of every reader that writes over HTTP or a WebSocket (`stream.mp4`, `frame.mp4`, MSE): a share of what the process may still take (its control group's limit less its use, `MemAvailable`, or on Windows and macOS the available physical memory), read again every 5 s, at least 32 MiB; or a size, `256` (MiB) or `"256MiB"`, `"2GiB"`. Each reader is bounded by the smaller of its class's queue and its share, never below one group of pictures; when the budget is full the reader furthest behind skips to its next keyframe. Anything else is refused at startup with what is expected |
| `api.gop_cache_mib` | 8 | the current group of pictures kept per source, in MiB: what a new consumer starts from and what a viewer's keyframe request is answered with. A group larger than it is not cached, and said once per source (`group of pictures larger than the keyframe cache`, with its size): 8 MiB is a 6 s group at about 10.6 Mbit/s (D16) |
| `api.idle_grace_ms` | 5000 | how long a source with no consumer left keeps running, in milliseconds (0 to 3600000): a camera's session, which a viewer who comes back within it finds open; go2rtc stops a source with its last consumer |
| `api.transcode_idle_grace_ms` | 0 | the same for an `ffmpeg:` source that encodes (a `#video=` or `#audio=` other than `copy`, a `#raw=`, a filter; one that only copies is a camera read through FFmpeg and keeps `api.idle_grace_ms`), which costs a core and 70 to 110 MiB while it waits: stopped with its last consumer, as go2rtc's; its next start is 0.1 to 0.3 s for a camera the recorder holds (D36) |
| `rtsp.queue_max_mib` | 32 | what one RTSP reader's queue holds, in MiB (a recorder, D15) |
| `rtsp.queue_max_seconds` | 10 | how long its oldest frame may wait; beyond either, it resumes at a keyframe |
| `webrtc.queue_max_mib` | 8 | the same for a live consumer: a WebRTC viewer, the API's stills and preloads, and every RTSP reader that says `queue=live` (openIVR's analysis decoder, the `ffmpeg:` driver's own read for a video transcode, D18): one budget for all of them, so tightening it for viewers tightens the transcodes and the detector too |
| `webrtc.queue_max_seconds` | 2 | its age bound |

The limits are whole numbers of at least 1 (the streams' and queues' sizes in
MiB up to 4096, their times in seconds up to 3600; `ffmpeg.max_memory_mib` has
no ceiling, a larger value only lifting FFmpeg's address-space limit); a per-address one is effective up
to its listener's total (the API and RTSP totals, 256 each, are fixed). Loopback is counted like any address, a global or
unique-local IPv6 peer by its `/64` (so IPv6 viewers on one LAN subnet share
one bound), a link-local IPv6 peer by its own address.

## HTTP API

All on `api.listen`. JSON bodies are UTF-8; request bodies are capped (64 KiB,
SDP included). At most 256 connections are open at once (more wait in the
accept queue, and reaching the bound is logged at most once a second) and 128
from one address (more are closed on accept). A
request's head (request line and headers) must arrive within 10 s of its first
byte, or the connection closes without an answer; a connection silent for 75 s
closes. A connection the server closes is closed in two steps (D31): the
answer and the end of the server's side first, then what the client still
sends is read and dropped (2 s and 256 KiB at most) - so a refusal sent before
the request's body arrived (`401`, `404`, `415`...) is read by a client that
was still writing, where a plain close reset the connection and, on Windows,
took the answer with it.

| Call | Answer | Notes |
|---|---|---|
| `GET /api` | `200 {"version": "...", "output_memory": {...}}` | health and version; `output_memory` (oxrtc only, D45): `budget`, `held` (bytes), `readers`, `skips`, the host's `limit` and `available` at the last reading and their `source` |
| `GET /api/ws?src=N` | `101` | go2rtc's WebSocket API: see "WebSocket API" below |
| `GET /api/streams` | `200 {name: StreamInfo}` | `?src=name` for one stream (`404` if absent) |
| `PUT /api/streams?name=N&src=A&src=B` | `200` | create or replace; same sources = no-op; `400` with a reason for a source no driver accepts |
| `DELETE /api/streams?src=N` | `200`, `404` | openIVR treats a failed delete of `_sub`, `_rec` and `_src` as normal. go2rtc answers `400 yaml: path not exist` for a name never defined and `200` for one deleted before; openIVR reads only "200 or 204" of the main stream's |
| `PUT /api/preload?src=N[&video=…][&audio=…]` | `200` held, `500` with a reason | see below. **An unknown stream is `500 streams: stream not found: N`, never `404`**: openIVR reads a `404` of a preload as "this go2rtc has no preload" and holds nothing until it restarts |
| `DELETE /api/preload?src=N` | `200` | releases; a stream held by nothing is `200` too (go2rtc: `500 streams: preload not found`; openIVR ignores the answer) |
| `GET /api/preload` | `200 {name: {...}}` | listing |
| `POST /api/webrtc?src=N` | `201` (or `200`) `application/sdp` answer | body: the SDP offer; `404 stream not found`; `500 streams: …` when no source opens. **oxrtc adds** `Location: /api/webrtc/<id>` and `Accept-Patch: application/trickle-ice-sdpfrag` (go2rtc sends neither): the session's resource, below |
| `PATCH /api/webrtc/<id>` (**oxrtc only**) | `204` | body `application/trickle-ice-sdpfrag` (`415` otherwise): the candidates a client trickles; `404` for an unknown, ended or malformed id, `400` for more than 16 candidates in one request or 64 for a session, `501` for an ICE restart (another `a=ice-ufrag` than the offer's). go2rtc answers `405` |
| `DELETE /api/webrtc/<id>` (**oxrtc only**) | `200` | ends the session; returns once (2 s at most, if its task is stuck on a socket) its consumer has left `GET /api/streams` and its places count again; `404` for an unknown or ended id. go2rtc's `DELETE /api/webrtc?id=` is for its WHIP sessions and answers `404` for a WHEP consumer |
| `GET /api/frame.jpeg?src=N&width=W` | `200 image/jpeg` | anything else is a failure to openIVR |
| `GET /api/stream.mp4?src=N[&mp4[=flac\|all]][&video=…][&audio=…][&duration=S][&filename=F]` | `200 video/mp4; codecs="..."`, chunked | a progressive fragmented MP4: see "MP4 over HTTP" below |
| `GET /api/frame.mp4?src=N[&filename=F]` | `200 video/mp4; codecs="..."` with `Content-Length` | one keyframe as an init segment and one fragment: see "MP4 over HTTP" below |
| `PUT /api/webrtc/ice` (**oxrtc only**) | `200 {"turn": N, "stun": M}` | body: a JSON list of `{urls, username, credential}`, the shape of `webrtc.ice_servers` (at most 16 entries of 16 URLs, 1024 bytes a field); replaces the ICE servers of the running WebRTC egress for the sessions made from now on, and cuts none (see WebRTC). `N` and `M` count the TURN and STUN servers that were usable. `400` for a body that is not that list (the answer never quotes the body: it holds a credential), `404` from go2rtc (openIVR falls back to a restart), `501` when WebRTC is off |

`StreamInfo`, the fields openIVR reads (others may be added):

```json
{
  "producers": [{
    "url": "rtsp://***@192.168.1.100:554/live/ch00_0",
    "medias": ["video, recvonly, H265", "audio, recvonly, PCMA/16000"],
    "receivers": [{"codec": {"codec_name": "pcma", "codec_type": "audio", "clock_rate": 16000}, "packets": 1234}]
  }],
  "consumers": [{
    "format_name": "preload",
    "medias": ["audio, sendonly, OPUS"],
    "remote_addr": "127.0.0.1:53211"
  }]
}
```

- A producer appears per **running** source; `medias` is empty or absent until
  the source has connected. Direction is seen from the server: `recvonly` is
  what it receives from a camera, `sendonly` what it sends to a consumer.
- `receivers[].packets` counts received packets (frames are acceptable) and is
  **omitted while 0**.
- `consumers[].format_name` is `preload` for a preload, `webrtc/whep` for a
  viewer whose offer came through `POST /api/webrtc` (go2rtc's name), `rtsp`,
  `mse` otherwise. openIVR decides whether a camera is held by looking for
  `preload`. A consumer whose request carried a `User-Agent` lists it as
  `user_agent` (cleaned, at most 256 characters; a header that is not text is
  left out): openIVR names each WHEP session `openIVR-whep/<session id>`. A viewer's `medias` is empty here where go2rtc lists the tracks it sends (`video, sendonly, H264`, measured 2026-11-16); openIVR reads `medias` of preloads only.
- oxrtc only, ignored by go2rtc clients: `consumers[]` also carry `queue`
  (`live` or `recorder`: the budget the consumer reads with, so a reader's
  `queue=` can be checked from outside), `frames`,
  `bytes` and `dropped` (lost because the consumer fell behind its queue's
  budget: 10 s or 32 MiB for an RTSP reader, 2 s or 8 MiB otherwise, by
  default: `rtsp.queue_*` and `webrtc.queue_*`), and each stream a
  `sources` list with **every** source in order, running or not: `url`
  (redacted), `state` (`idle`, `connecting`, `running`, `retrying`, `failed`),
  `error` (why the last attempt failed, redacted, until a run delivers media
  again), `failures`, `last_frame_age_ms`, `frames`, `bytes`, `rejected`,
  `demand`. go2rtc shows a source that keeps failing as nothing at all. An
  alias also carries `reads`, the stream it names: its other fields are then
  the source it reads there, and as a producer it lists the tracks it passes.
- A relay is not a consumer of the stream it reads: go2rtc lists its loopback
  RTSP session as an `rtsp` consumer of the upstream, oxrtc lists nothing
  there (openIVR reads the upstream's `producers` and its own stream's
  `preload`, not that consumer), and the upstream's consumer limit does not
  count relays.

### Preload

`PUT /api/preload?src=camera_N_rec&audio=opus` must answer **200 only once the
selected sources have connected** (their tracks known) and **500** when a wanted
media cannot be produced (the sound transcode fails, the camera has no
microphone): openIVR then retries with `video=` alone. A second `PUT` of the same
stream replaces the first preload **once it holds**: the old one is released
only then, so the camera is never left unheld between the two, and a second
`PUT` that fails (`500`) leaves the first one holding. go2rtc removes the old
preload before it walks the new one's sources (openIVR's `_regain_sound`
copes with both: after a `500` it asks for `video=` again). Meanwhile
`GET /api/preload` lists the hold, and `GET /api/streams` shows two `preload`
consumers, both with the hold's `medias`. Of two `PUT`s still starting, the
later one wins and the earlier answers `500` (`replaced`). go2rtc serialises
every preload behind one global mutex; oxrtc must not (a failing camera must
not delay the others).

## RTSP server (`rtsp.listen`)

- At most 256 connections at once and 128 from one address; one beyond either
  is closed on accept. A request is at most 64 KiB, headers and body together
  (`413` and the connection closes beyond), however slowly it arrives, and
  must arrive within 10 s of its first byte (`408` and the connection closes
  beyond; go2rtc has no such deadline). A connection silent for 60 s closes.
- `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`, `GET_PARAMETER`
  (keep-alive). **TCP interleaved is required** (every openIVR reader uses it);
  UDP is optional.
- Path: `/<stream name>`; query: the consumer's want - `?video`, `?audio`,
  `?video=copy`, `?audio=opus`, `?video=h264&audio=opus`. A query naming one
  media excludes the other. Tracks are described video first.
- Motion JPEG (a webcam, "Webcams" below) is described `m=video 0 RTP/AVP 26`,
  `a=rtpmap:26 JPEG/90000`, and sent as RTP/JPEG (RFC 2435), with the
  quantization tables in the first packet of every picture. FFmpeg decodes it
  to the very picture of the source; a picture with Huffman tables of its own
  is coded again with the standard ones, losslessly; a picture RFC 2435 cannot
  describe is not sent (R42). go2rtc describes it with payload type 96.
- `queue=live` or `queue=recorder` in that query names the reader's class
  (oxrtc only, D18; go2rtc ignores keys it does not know). Without it an RTSP
  reader is a recorder (10 s or 32 MiB, `rtsp.queue_*`); a reader that shows
  what it reads, such as openIVR's analysis decoder (and the `ffmpeg:` driver,
  for a `#video=` transcode with no `#audio=`, R23b), says `?video&queue=live`
  and is cut to a keyframe after 2 s or 8 MiB (`webrtc.queue_*`) instead of
  being left seconds behind. The price is paid by a reader that really
  stalls: past 2 s, with the camera's latest keyframe older than that (the
  owner's cameras send one every 6 s), there is nothing to resume at, and the
  reader is shown no picture until the camera's next keyframe, up to 6 s
  (`crates/core/tests/queues.rs`
  `a_live_reader_stalled_on_a_six_second_gop_waits_for_the_next_keyframe`). A
  decoder that cannot keep up with the picture it reads (the main stream
  where no substream is declared, on a small machine) therefore sees
  recurring blackouts where it used to see a lag growing to 10 s, and so can a decoder that needs over 2 s to consume the cached group of pictures a new reader is given first (not measured, D18's review). Any other value, or two different ones, is
  refused with `400`. The reader's `User-Agent` is listed as the consumer's
  `user_agent`.
- `ANNOUNCE`/`RECORD` (publication) for transcoders only, on one-time path
  tokens; never open to arbitrary clients.
- **Authentication** (`rtsp.username`/`rtsp.password`, R28, D24), measured on
  go2rtc 1.9.15 and compared:

  | | go2rtc 1.9.15 | oxrtc |
  |---|---|---|
  | Challenge | one `WWW-Authenticate: Basic realm="go2rtc"` | two lines: `Digest realm="oxrtc", nonce=..., algorithm=MD5, qop="auth"`, then `Basic realm="oxrtc"`; FFmpeg takes the stronger, a Basic-only player the second |
  | Accepted | `Basic` only (the scheme word is case sensitive; a Digest answer is refused) | Basic (scheme not case sensitive), Digest MD5 with `qop=auth` or RFC 2069's none |
  | Asked of | every request of a peer **not on loopback**, `OPTIONS` and `TEARDOWN` included; loopback peers (`127.0.0.1`, `::1`) are let in with no check at all | every request of every peer, loopback included, unless `rtsp.local_auth: false` |
  | Wrong credential | `401`, then the connection closes | `401` after 250 ms, then the connection closes; one warning per 10 s with a count, never the credential |
  | None given | `401`, the connection stays open for the retry | the same |
  | Publication (`ANNOUNCE` on `_publish/<token>`) | asked like the rest | not asked: the 128-bit token is its credential, and it is taken from loopback only |

  A Digest nonce belongs to the connection that was given it: an answer made
  for another one (a player reusing its last connection's) is a `401` with
  `stale=true` and a new nonce, and the connection stays. An `Authorization`
  header over 1,024 bytes is refused.
- **A run's own credential** (D24). When the server asks for a credential, each
  publication token also carries a user name (`run-` and 64 random bits) and a
  password (128 random bits) of its own, valid for reading until the token's
  handle is dropped. The `ffmpeg:` driver puts it in the input URL of a source
  that reads this server (`ffmpeg:camera_1_src#...`, or the relay's own URL
  `ffmpeg:rtsp://127.0.0.1:8554/camera_1_src#...`, which is how openIVR writes
  its sound transcode; `localhost` on the relay's port is the same relay when
  it listens on loopback, as the RTSP client reads it), so FFmpeg's command
  line, its error lines and a process listing never show the operator's
  password, and a credential seen there ends with that run. A URL that names its own user is left as written; a camera's
  URL is never touched. Sources between streams (`rtsp://127.0.0.1:8554/x` as a
  stream's source, openIVR's `_camera_video`) are in-process aliases (D14) and
  read nothing over the wire, so they need none.
- A session idle between `SETUP` and `PLAY` for more than 5 s may be dropped
  (go2rtc's behaviour; openIVR waits for it).
- A reader that stops reading loses nothing for 10 s or 32 MiB of media,
  whichever comes first (a recorder's queue, ARCHITECTURE "Channel"); beyond,
  it resumes at a keyframe, with a warning in the log (`consumer fell behind
  its queue budget`, at most once per 10 s per reader, with the count). A
  single write blocked for `session_timeout` (60 s) still closes the
  connection. go2rtc keeps no such budget for its RTSP readers.
- RTP time does not jump when a source restarts (a camera reopened, a sound
  transcode restarted): it is the hub's timeline (ARCHITECTURE, D12), where
  go2rtc passes each source's timestamps through and a long-lived reader
  takes the restart for elapsed time (openIVR's segmenter measured a sound
  9.4 hours ahead of its picture). Each track's first RTP packet comes right
  after an RTCP sender report on the track's RTCP channel, dated by when its
  frame was received, then one every 5 s, so a reader that dates packets by
  the reports (FFmpeg, for two tracks or more) places picture and sound on one
  clock from the first packet.

## Sources

| Spec | Meaning |
|---|---|
| `rtsp://…`, `rtsps://…` | a camera, over TCP; `#timeout=N` = stall timeout in seconds (a ceiling: a camera that has shown its cadence is restarted after four times its longest gap between two frames, at least 10 s, D34; go2rtc waits the whole timeout, then retries every 1 s five times, then every 5 s, 10 s and 1 min). A status other than 401 and 404 is `sources[].error` `busy: 461 response to SETUP ...`, retried on the backoff, never twice a second (D34). `rtsps` is RTSP inside TLS (port 322 when the URL names none: go2rtc 1.9.15 dials 554 there, so a hand-written `rtsps://host/path` for a camera that speaks TLS on 554 needs `:554`; openIVR's promoted URLs always carry the port); its certificate is **not checked** unless the source says `#verify` (D20: go2rtc 1.9.15 checks nothing, and openIVR's TLS probe writes a bare `rtsps://` URL for cameras whose certificate is their own). `#verify` uses the operating system's roots and the host the URL names; `#insecure` is accepted and changes nothing (it is what `rtsps` already does); `#verify` with `#insecure` is refused with `400`. Without `#verify` nothing about the certificate is checked, its key size and signatures included (a self-signed 1024-bit RSA one plays, as on go2rtc); TLS 1.2 or 1.3 with an ECDHE or TLS 1.3 suite is needed (go2rtc's limit too). A TLS failure is in `sources[].error` (`unreachable: Unable to connect to RTSP server: TLS: invalid peer certificate: UnknownIssuer`; a camera that drops the connection: `the camera closed the TLS connection without ending it (no close_notify)`), the URL's password never is |
| `rtsp://127.0.0.1:<own port>/<name>?<query>` | another stream of this server: becomes `stream:<name>?<query>`, no network hop; its `#timeout=N` is accepted and has no effect |
| `stream:<name>?<query>` | an alias of stream `<name>`, passing what `<query>` asks (`video`, `audio=opus`...): its consumers read that stream's own source, resolved when they subscribe (D14). A stream that would read itself, directly or through other streams, is refused (`400`, `a stream cannot read itself through b -> a`) |
| `ffmpeg:<stream name>#video=h264#width=W#height=H` | transcode of a stream of this server; declares `video/h264` |
| `ffmpeg:<url>#audio=opus#input=<template>#timeout=N` | transcode of a URL (rtsp, http) with a named input template; declares `audio/opus` |
| `http://…`, `https://…` | a webcam serving Motion JPEG (below); `#timeout=N` = stall timeout in seconds (a ceiling, as for `rtsp`: D34), `#interval=MS` = pause between two requests of a camera that serves one picture per request (10 to 60000, default 100), `#insecure` = do not check an `https` camera's certificate; declares `video/jpeg` |

`exec:`, `echo:`, `expr:` and every scheme not listed are refused with `400`.

### Webcams (`http(s)://`, D19)

`GET` on the URL, then what the response's `Content-Type` says it is:

- `multipart/x-mixed-replace`: the camera's stream, one JPEG per part, for as
  long as the response stays open. The boundary is the declared one (its
  leading dashes optional, as cameras disagree on them), or the first
  `--` line when none is declared. A part with a `Content-Length` is
  delivered when its last byte arrives, one without it when the next
  delimiter does. Junk between parts, parts that are not images and parts
  of 0 bytes are dropped; a part over 8 MiB, headers over 8 KiB or a
  closing delimiter end the run (it is started again with the usual backoff).
  HTTP/1.0 with no length, chunked HTTP/1.1, bare line feeds and the
  sloppy header lines of small cameras are read.
- `image/jpeg`, `application/octet-stream` or no type: one picture per
  request. go2rtc asks again the moment it has the picture (measured on
  1.9.15: dozens of requests in the same millisecond); oxrtc starts a
  request every `#interval` (100 ms by default, so at most ten pictures a
  second), on the connection the camera keeps open or a new one.
- anything else (`text/html`...) is an error naming the type, nothing of
  the body. A body that does not start with `FF D8` is skipped; a source
  that never gives a picture for the stall timeout is restarted.

Credentials come from the URL (`http://user:pass@host/…`, percent-decoded)
and answer a `401` as Basic or Digest (the algorithms of the `http-auth` crate
that retina uses too), at most once per request; they go to the origin the URL
names and to no other (a redirect to another scheme, host or port drops them).
Up to 5 redirects are followed. A TLS connection that ends with no `close_notify` is the end of the data (`eof.rs`), as in Go: the owner's `https` camera answers `HTTP/1.0` and ends a body by closing its socket, and go2rtc 1.9.15 gave a picture with no length from such a server (measured: 9057 bytes) where oxrtc, strict as rustls is, answered `unreachable: error reading a body from connection` forever. A body with a `Content-Length` is still counted. `https` verifies the certificate against the
operating system's trusted roots and the host the URL names. **go2rtc 1.9.15
verifies nothing** (measured: it served a picture from a self-signed local
server), so a camera with a certificate of its own that works there is refused
here (`TLS: invalid peer certificate: UnknownIssuer`) until its source says
`#insecure`, which takes any certificate and logs a warning at each connection.
The track is `video, recvonly, JPEG` in `/api/streams`: no sound, which is
what openIVR's `_has_sound` reads.

**Checked on the owner's three webcams (2026-11-16, one session each, a few
seconds, the binary in openIVR's layout).** `https://wc-heli.chuv.ch/view/`
(`camera_4_src`) answers one `GET` with `HTTP/1.0 200`, `Connection: close`,
`Content-Type: multipart/x-mixed-replace; boundary=myboundary`, then parts with a
`Content-Length` and the Huffman tables of the standard (four `DHT` segments, 418
bytes): an Axis camera, baseline 4:2:0 JPEG of 1024x640, one picture a second
(about 86 KB each). Its certificate is accepted by oxrtc against the operating
system's roots (Windows; the chain is complete, no `#insecure`), so the one `https`
camera needs no change at the switch. The two `http` ones (`camera_5_src`,
`camera_11_src`, `/mjpg/video.mjpg`) are read as `mjpeg` 704x480 and 704x576, `video,
recvonly, JPEG` in the listing. ffprobe of `camera_N_src?video` over RTSP sees all
three as `mjpeg`/`yuvj420p`; it needs 20 s on the one-picture-a-second camera, and
go2rtc 1.9.15 needs the same 20 s on a replay of the bytes captured from it (ffprobe's
own analysis of a 1 fps stream, not the server). On the same replay, the public
stream's H.264 transcode (`ffmpeg:camera_4_src#video=h264#width=1920#height=1080`)
came in 4.2 s through oxrtc and 10.1 s through go2rtc. A source whose last reader has
left keeps its connection for `idle_grace` (5 s, `api.idle_grace_ms`; a transcode of it stops with its last viewer, D36); a camera that counts its
connections (this one answered `503` to a `GET` made about a second after the previous
one was closed; the run ended before the retry, 0.5 s later, could be watched) is asked
again after the backoff, as any failed run is. The pinned shape of
that answer is `the_head_of_a_public_axis_webcam_is_read_as_it_is_sent`.

Read back over RTSP (`rtsp://127.0.0.1:P/camera_N_src?video`: the H.264
transcode, the recorder's re-encode and the analysis all read it so) a
JPEG is sent as RTP/JPEG (RFC 2435, payload type 26, `m=video 0 RTP/AVP 26`, with
`a=framerate:<pictures a second>` once its rate is known, however fast: the mean
of its newest gaps that are no pause, a whole number of pictures a second when
within 2.5 % of one, and none above 120 a second (pictures that arrived together
are the network's, not the camera's rate) (until 2026-11-20 only when it sent ten a second or fewer,
or when its dates sat on a grid: the owner chose to give the encoder a webcam's
rate on every start, D36); go2rtc announces none; an H.264 or H.265 track says
its pace too, whatever it is, once three gaps between its pictures were seen;
and a track whose dates sit on a grid - a camera with a clock of its own that
stamps 25 slots a second, whether it fills each or one in three, or a webcam
of 25 or 30 a second whose pictures come evenly that minute - says the
grid's rate, whatever its codec, as a whole number of pictures a second when
the dates fit one: what FFmpeg's own guess reads of it),
the pictures as they are, but for the Huffman coding. RFC 2435 describes a
picture by a few numbers and leaves the rest to the receiver, which decodes
with the standard Huffman tables of ITU-T T.81 Annex K (Y on the luminance
ones, the chroma on the chrominance ones). A picture coded with tables of its
own (FFmpeg's `-huffman optimal`, libjpeg's `optimize_coding`, a file that
numbers or assigns its tables otherwise) has its scan coded again with the
standard ones, symbol by symbol, the coefficients' raw bits and the restart
markers copied, so the receiver decodes the very picture of the file (FFmpeg's
MD5 of the file and of the RTP read are equal in `tests/jpeg.rs`); the scan
grows by 7 to 16 % on the test pictures, and a picture whose scan does not decode with its own
tables is dropped. What RFC 2435 cannot describe at all is not sent (a gap,
never a corrupt picture; one warning per reader): baseline (not progressive)
with 8-bit samples, three components in the frame's order with Y at 2x1 or 2x2
and the chroma at 1x1 (4:2:2 as libjpeg writes it, 4:2:0), 8-bit quantization
tables, a side of at most 2040 pixels (R42). Bytes after the end of image
(a camera's padding) are not sent. `frame.jpeg` of such a stream returns the
latest picture as it is, whatever it is.

A camera's name is resolved to every address it has and they are tried in
turn, the families alternating, each but the last for a third of the connect
timeout (two seconds at least): an unreachable IPv6 address does not use up
the time of the IPv4 one that works (`TcpStream::connect` on a name waits for
each address as long as the system does; Go's dialer, go2rtc's, falls back
within a fraction of a second).

The `ffmpeg:` templates are go2rtc's, with one difference (D11): the `rtsp`
input template has no `-fflags nobuffer`, which made every transcode wait for
the camera's next keyframe. A file that names its own `rtsp` template keeps it.

The second one (D18, R23b): a source with `#video=` and no `#audio=` that
reads a stream of this server (`ffmpeg:camera_N_src#video=h264...`) reads it
through `rtsp://<relay>/camera_N_src?video&source=ffmpeg:...&queue=live`, so
its reader is cut to a keyframe after the live budget (2 s) where go2rtc's
(and an `#audio=` transcode's here) falls up to 10 s behind. go2rtc ignores
the key. A URL input is read as written, and a `#query=queue=recorder` brings
the old class back.

The third one (W5, D37): an `#audio=` transcode of a G.711 or L16 sound that
oxrtc can read itself - a stream of this server, its loopback RTSP URL, an
`http(s)` URL answering `audio/wav` - starts no FFmpeg. What it produces is
what FFmpeg produced with go2rtc's template: Opus described `opus/48000/2`
(one channel, 64 kbit/s, 20 ms packets, the low-delay application), or G.711
and L16 at the template's rate. The source's other fragments, `#raw=`, an
operator's own audio template, `opus/16000`, an `#input=` that does more than
probe, and an input that turns out to be AAC, Opus or another WAV coding are
FFmpeg's as before. Two visible differences: the transcode reads the stream
as a consumer named `native_audio` (where FFmpeg was an `rtsp` reader with
go2rtc's user agent), and a stream with no sound declines the source (a
`SourceError::Declined`, slow backoff) where FFmpeg's run ended unreachable.
`ffmpeg.native_audio: false` brings FFmpeg back for every source.

## WebRTC

- At most 64 sessions at once (`webrtc.max_sessions`) and 32 for one caller
  address (`webrtc.max_sessions_per_ip`, the HTTP peer of `POST /api/webrtc`);
  beyond either the offer is answered `503`.
- WHEP over `POST /api/webrtc`. The answer is complete (no trickle needed): host
  candidates for every local address plus `webrtc.candidates`, on the single
  UDP port of `webrtc.listen`; ICE-TCP passive candidates (`tcptype passive`)
  at the same addresses on a TCP listener at that port, as go2rtc listens on
  both (a TCP port that cannot be bound is warned about, and only TCP is
  lost); and, from `webrtc.ice_servers`, a server-reflexive candidate per
  `stun:` server and a relayed one per `turn:` server.
- One `MediaStream` per session (W1, D30), as go2rtc and MediaMTX answer:
  every media line of the answer names the same stream in `a=msid:<stream>
  <track>` and in the `msid:` attribute of each `a=ssrc` (the picture's SSRC
  and its RTX SSRC), and `a=msid-semantic: WMS <stream>` lists it alone, so a
  player that takes `event.streams[0]` at every `track` event shows the
  picture. go2rtc's stream id is the constant `go2rtc` (tracks `go2rtc-video`,
  `go2rtc-audio`), MediaMTX's `mediamtx` (`video`, `audio`); oxrtc's is 32 hex
  characters drawn per session, its tracks `<stream>-0`, `<stream>-1`... by
  media line: a client must not expect a fixed name. One stream is also what
  makes a browser hold the picture for the sound, as with the two others:
  about 60 ms on a camera whose sound is regular (L01), about 0.6 s on one
  whose sound stalls (the owner's camera_1: 679 ms held against 57 ms with
  0.1.x's stream per track; go2rtc 822 ms, MediaMTX 972 ms; D30). Since W1b
  a stall's sound beyond `webrtc.max_sound_late_ms` is not sent (the sound is
  otherwise sent as it comes; W1b's pacing is off by default since its
  review): 462 ms on that camera against 896 (go2rtc) and 1540 (MediaMTX)
  in the same minutes (measured with the pacing on); `webrtc.lip_sync: false` adds the `playout-delay`
  header extension (0 to 0) to the picture, which the answer accepts only when
  the offer names it, and the browser no longer holds the picture.
  Since W1c the two tracks' RTCP sender reports date the sound
  `webrtc.sound_lag_ms` (20 ms) after the picture that arrived with it, with
  lip sync on: a client that reads capture times from the sender reports sees
  the picture's 20 ms before its arrival at oxrtc and the sound's at its
  arrival; `0` dates both at their arrival, as before.
  The sound a viewer hears starts at its connection: what the camera sent
  between the answer and the end of ICE and DTLS is not sent late.
- `webrtc.ice_servers` as openIVR writes it (`set_ice_servers`: one entry,
  its relay's `stun:` and `turn:` URLs, a `username` and `credential` that
  rotate; go2rtc is restarted with each new pair, oxrtc is given it with
  `PUT /api/webrtc/ice`, below): `stun:host[:port]`
  and `turn:host[:port][?transport=udp|tcp]` are used (port 3478 by default, the
  name resolved at each allocation); `turns:` and `stuns:` (TLS to the server)
  are logged at startup and not used (D13); a malformed URL is warned
  about by its position, never printed. A `turn:` URL without a username and
  credential is not used. At most four TURN and four STUN servers (openIVR's
  entry takes two of the four: its UDP URL and its TCP one are two allocations
  and two relayed candidates). `?transport=tcp` is one TCP connection to the
  server per allocation, the relayed side still UDP (D21): the messages follow
  one another with no length in front (RFC 8656 section 3.1, not RFC 4571's
  prefix), a request is sent once, ChannelData is padded to four bytes, and a
  connection that fails, closes or speaks something else ends the allocation,
  made again on a new connection after the same backoff as over UDP; a relayed
  message beyond 16 KiB is dropped, as a UDP datagram that long would be, and
  does not end the allocation. The TURN
  allocation is made at startup and kept; an offer answered in the first 2 s
  after startup waits for it, until then at most. A relayed or reflexive candidate makes the session full ICE
  instead of ICE-lite. Neither the user name nor the credential is ever
  printed.
- **`PUT /api/webrtc/ice`** (oxrtc only, D22) replaces `webrtc.ice_servers`
  while the server runs. A STUN or TURN server that is already in use with
  the same credential is left alone (the same call twice changes nothing). A
  TURN server with another credential gets a new allocation, on a socket or
  connection of its own, beside the old one: the old one keeps being offered
  until the new holds, then is **retired**: no session made from then on is
  offered its relayed address, and the sessions that were keep it, their media
  flowing through it, until none of them is left (the allocation is then
  released at the server, at least 60 s after it was retired:
  `WebRtcConfig::ice_retire_grace`, not a configuration key). A credential the server refuses never holds: the old
  allocation stays the one offered (also when the same refused credential is
  sent again), and the refused one is tried again as any allocation is. A server that is no longer listed is retired at once; one
  added is allocated at once. At most eight retired allocations are kept; past
  them the oldest ends with its sessions (a rotation every few minutes never
  reaches that). An old allocation that fails (its credential expired and its
  refresh was refused) is not made again with that credential: the sessions
  that had it lose the relayed path, as they would have lost everything at a
  restart. The configuration file is not written: the next start reads what
  the file says, which openIVR rewrites at each rotation, as before.
- The offer's codecs choose the source: a browser offering H.265 gets the
  camera's own stream; one offering only H.264 gets the transcode. Audio: Opus
  (PCMA/PCMU when offered and the source has them).
- An offer that shares no codec with the stream is refused `400` (on
  `/api/ws`, `{"type":"error","value":"webrtc/offer: ..."}`) with go2rtc's
  words, which name the stream's codecs, then the offer's: `streams: codecs
  not matched: video:H264, audio:OPUS => video:VP8, audio:OPUS` (D43). An
  offer whose video lists no codec oxrtc sends (VP8, VP9 or AV1 alone) is
  refused so when the stream has a picture, even if its sound could be sent:
  go2rtc 1.9.15 refuses it `500 unable to populate media section, RTPSender
  created with no codecs` and MediaMTX 1.21.3 `400 codecs not supported by
  client`; oxrtc before answered the sound alone, which aiortc refused. To a
  stream with no picture the same offer gets the sound, its video line
  answered on the offer's own codec with nothing sent on it, as MediaMTX
  answers (go2rtc refuses it).
- H.265 (W7): the camera's own picture goes to a browser one of whose H.265
  payload types takes the camera's profile, tier and level (RFC 7798
  `profile-id` equal to the SPS's or one its compatibility flags name - a
  Main camera plays on a Main 10 type, whatever its flags say, as H.265
  A.3.3 has every Main 10 decoder decode Main -, `tier-flag` no higher, `level-id` at
  least its `general_level_idc`; no `fmtp` means Main, Main tier and no
  level ceiling, an absent `level-id` no ceiling, D41 as amended 2026-11-23),
  and no
  transcoder starts for it. Otherwise (a level above the offer's, a Main 10
  camera to a Main-only offer, the High tier) it gets the stream's next
  source without H.265, openIVR's H.264 transcode; a stream with no such
  source still sends the camera's H.265, answered on the offer's first H.265
  payload type (whatever the camera's profile: a Main 10 camera to a
  Main-only offer got an answer with no video at all until the W7 review).
  The answer names the camera's own
  profile, tier and level on the offer's payload type of that profile
  (`a=fmtp:49 profile-id=1;tier-flag=0;level-id=150` for the owner's XM
  cameras, `profile-id=2` on the Main 10 type for a Main 10 camera), where it
  said Main, level 6.0 whatever the camera. What the browsers of the owner's
  PC (RTX 4080) offer, measured 2026-11-22: Chrome 157, headed and headless
  and with no flag, offers `level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST`
  and the same with `profile-id=2`, and plays the camera's H.265 (720p from
  libx265, and the owner's XM camera at 2304x1296) with no FFmpeg started;
  Edge 157, headed or headless, offers no H.265 for WebRTC (its viewers, and
  bench/'s, get the transcode), and forced to with
  `--enable-features=WebRtcAllowH265Receive` its page crashed during the
  session. go2rtc 1.9.15 answers every H.264 and H.265 payload type it has,
  sends the H.265, and starts the transcode beside it; MediaMTX 1.21.3 copies
  the offer's `fmtp` and checks no level: both send a camera labelled level
  6.2 to Chrome's level 6.0, which decodes it (a 720p picture), where oxrtc
  sends the transcode as RFC 7798 reads the receiver's `level-id`. Safari 18
  is not checked (no Mac here). D41.
- A session no packet of its viewer ever reached ends `reason="no packet from
  the viewer reached this server"` (one that was reached and never connected,
  `ice never connected`) where it said `ice disconnected` or `idle timeout`,
  and the server warns at most once a minute that a firewall or the answer's
  candidates are the cause (`webrtc.listen`, `webrtc.candidates`; on Windows,
  a firewall rule for the oxrtc binary's own path, which go2rtc's rule does
  not cover).
- IPv6 candidates: openIVR strips them from both offer and answer; emit IPv4
  first.
- RTP time is the hub's timeline, as for RTSP: a source's restart moves a
  viewer's clock by the time the source was away, picture and sound alike.
- A viewer's keyframe request (PLI, FIR) is answered from the cached group of
  pictures, at most once a second per session, the frames re-stamped after
  the last ones sent (ARCHITECTURE "Keyframe requests"); go2rtc and MediaMTX
  have no answer to it for an RTSP camera, whose next keyframe the viewer
  waits for. Since W3 (D35) oxrtc too leaves the request to the camera when
  its next keyframe is due within 1 s, or when the viewer asks again within
  2 s of a replay, and then sends no picture until that keyframe (what it
  would send cannot be decoded). A
  session that connects more than 2 s after its offer resumes from the same
  cache rather than at the camera's next keyframe.

## WebSocket API (`GET /api/ws?src=<stream>`, D39, D40)

go2rtc's WebSocket API (its `internal/api/ws/README.md`), the signalling Home
Assistant's go2rtc integration, the WebRTC Camera component, Frigate's cards
and go2rtc's own player use. WebRTC (M5 F2), MSE (M5 F3) and MJPEG (M5 F5);
`hls` is served (F6, D47); `mp4` comes with F4's socket part. Measured against go2rtc 1.9.15 and checked
with go2rtc's player in headless Edge (WebRTC and `mode=mse`) and Home
Assistant's client library (`go2rtc-client` 0.4.0).

The upgrade is authenticated as every route (`Authorization: Basic` or
`Bearer` on the handshake, loopback included); a credential in the query is
not one. A handshake with an `Origin` that is not the server's is `403` (`api.origin`,
below). Messages are JSON text frames `{"type": ..., "value": ...}`:

| Client sends | Server answers | Notes |
|---|---|---|
| `{"type":"webrtc/offer","value":"<SDP>"}` | `{"type":"webrtc/answer","value":"<SDP>"}`, then one `{"type":"webrtc/candidate","value":"candidate:..."}` per candidate of the answer | go2rtc's player. The answer is complete, as WHEP's is; go2rtc's carries no candidate and sends them as messages only |
| `{"type":"webrtc","value":{"type":"offer","sdp":"<SDP>","ice_servers":[...]}}` | `{"type":"webrtc","value":{"type":"answer","sdp":"<SDP>"}}`, then the candidates as above | Home Assistant's library. `ice_servers` is not used: the server's own are (`webrtc.ice_servers`) |
| `{"type":"webrtc/candidate","value":"candidate:..."}` | nothing | before or after the answer (held until it, 64 at most); `a=` before the line is taken (a line still starting `a=` after it, `a=a=...`, is ignored: go2rtc's ICE parser refuses it); only the first line is read; an empty value ends the candidates and asks nothing |
| `GET /api/ws?src=<stream>&start=next` (**oxrtc only**) | an `mse` of that socket starts at the next keyframe instead of the cached one (D42 as amended); `start=cache` or none: the cached one; another value `400` before the upgrade | a page or card of its own (go2rtc's player opens `api/ws?src=` alone) |
| `{"type":"mse","value":"<codecs>"}` | `{"type":"mse","value":"video/mp4; codecs=\"avc1.42c01f,flac\""}`, then binary frames: the init segment, then one fragment per frame from a keyframe | go2rtc's player in `mse` mode (and the WebRTC Camera card). See "MSE" below |
| `{"type":"mjpeg"}` | `{"type":"mjpeg"}`, then one JPEG per binary frame | go2rtc's player in `mjpeg` mode, which shows each as the `<video>`'s poster. See "Motion JPEG" below |
| `{"type":"hls","value":"<codecs>"}` | `{"type":"hls","value":"<master playlist>"}`: go2rtc's master playlist of a new fMP4 session in the list's codecs (read as `mse`'s, the H.265 rule with them, D44), its media playlist at `hls/playlist.m3u8?id=`, which go2rtc's player rewrites to the server's `/api/hls/` and plays from a `data:` URL (F6, D47); an error before it (`hls: stream not found`, `hls: codecs not matched`) moves the player on to its next mode | as go2rtc (its handler answers the same master playlist) |
| `mp4` | `{"type":"error","value":"mp4: not supported by this server yet"}` | F4's socket part. go2rtc's player then plays WebRTC, MSE or MJPEG |
| any other type | nothing | as go2rtc |
| text that is not a message / a binary frame | close `1007` / `1003` | go2rtc closes with no close frame. The frame is read whole as JSON: a lone surrogate escape (`\ud800`) anywhere in it, even in a field no handler reads, makes it not a message (go2rtc reads it as U+FFFD; no client of go2rtc's writes one). Only an object is a message (`["mp4",""]` is not one), as for go2rtc. A key given twice is read at its last value, as go2rtc reads it |

A failed offer is `{"type":"error","value":"<type>: <reason>"}`, `<type>`
being the offer's (`webrtc/offer` or `webrtc`), the reason redacted:
`stream not found` (an unknown or absent `src`, go2rtc's words), `streams:
codecs not matched: <the stream's codecs> => <the offer's>` (D43),
`streams: <reason>` (a source that cannot open), `too many
sessions`, `bad offer: <reason>`, `an offer is already being answered`, `too
many offers on one socket`; and `webrtc/candidate: too many candidates
before the answer` or `webrtc/candidate: the candidate is too long` (over
1 KiB). The socket stays open.

- The session is the one `POST /api/webrtc` makes (its bounds:
  `webrtc.max_sessions`, `webrtc.max_sessions_per_ip`, the socket's peer
  being the caller), and it **outlives its socket**, as go2rtc's does: go2rtc's
  player closes the socket once the picture plays. A socket closed before its
  answer ends the session it asked for at once (go2rtc lists it 30 s).
- 128 sockets at once and 64 per address (`503` beyond); 64 KiB per message;
  50 messages a second, pings and pongs counted (one more: close `1008`); a
  candidate line of 1 KiB at most; one offer answered at a time,
  4 sessions in a socket's life; a ping every 20 s, and a socket silent for
  60 s (no pong) is closed `1000`; a message not written within 5 s ends the
  socket; the server's stop closes every socket `1001`. go2rtc has none of
  these: it pings nothing and a silent socket stays open (measured 130 s).
- `api.origin` (go2rtc's key): absent or empty, a page of the request's own
  `Host`, and when that `Host` names no port (a reverse proxy passing its
  host name on) a page of that host on any port, as go2rtc 1.9.15 (measured:
  `Host: 127.0.0.1:43384` with `Origin: http://127.0.0.1:9999` is `403`,
  `Host: 127.0.0.1` with it `101`); `"*"`, any page; any other value, a page
  of exactly the request's `Host`. A handshake with no `Origin` (a program,
  Home Assistant's library) is always taken. The rule is go2rtc's and stays
  (D39, confirmed by the owner 2026-11-23). A refused page gets `403` with
  the cause and the ways out in its body (the browser's network panel shows
  it; a page's `WebSocket` object sees only a failed connection), and the
  log says the same at `warn`, once per interval:

      origin not allowed: a page of http://nvr.lan:8000 may not open a websocket of host 127.0.0.1:1984 (api.origin); a reverse proxy passes the browser's Host header on (nginx: proxy_set_header Host $http_host; Caddy does by default), or api.origin: "*" lets every page in

  go2rtc 1.9.15 answers `403 Forbidden` and logs `websocket: request
  origin not allowed by Upgrader.CheckOrigin` with the host and origin
  at `ERR`, for every refusal. Behind a reverse proxy, the proxy passes the browser's
  `Host` (and the WebSocket upgrade) on:

      # nginx: the default Host is the upstream's ($proxy_host), which a page of the proxy's name is refused against
      location /oxrtc/ {
          proxy_pass http://127.0.0.1:1984/;
          proxy_http_version 1.1;
          proxy_set_header Host $http_host;
          proxy_set_header Upgrade $http_upgrade;
          proxy_set_header Connection "upgrade";
          proxy_read_timeout 120s;   # oxrtc pings every 20 s; nginx's 60 s default holds
      }

      # Caddy 2: reverse_proxy passes Host and the upgrade on by default
      nvr.example {
          handle_path /oxrtc/* {
              reverse_proxy 127.0.0.1:1984
          }
      }

  A proxy that cannot pass `Host` on (aiohttp's client, a CDN) sets
  `api.origin: "*"` instead, the credential then being the only guard (as
  for a program). An `Origin` is never rewritten by either proxy.

### MSE (`{"type":"mse"}`, M5 F3, D40)

The value is the codec list the browser's `MediaSource.isTypeSupported` took
(go2rtc's player: `avc1.640029,avc1.64002A,avc1.640033,hvc1.1.6.L153.B0,mp4a.40.2,mp4a.40.5,flac,opus`
filtered). Each name is read by its family for the sources it picks; an
H.265 entry's profile, tier and level (ISO/IEC 14496-15 annex E,
`hvc1.1.6.L153.B0` is Main at level 5.1) then say whether the camera's own
H.265 goes to the reader, by WebRTC's rule (D44): the camera's H.265 when an
entry takes its profile, tier and level, else the stream's next source
without H.265 (openIVR's H.264 transcode, which does not start for a reader
that takes the camera); with no such source, the camera's H.265 anyway. An
entry that is no annex E string, or the family alone, names no limit; an
entry without a level, no level ceiling. With go2rtc's player (whose list is
fixed) a Main 10 camera, or one above level 5.1, gets the transcode; go2rtc
1.9.15 sends it its H.265. The owner's cameras (Main, 5.0) get their H.265. By family:

| Listed | Taken from the stream | Written as |
|---|---|---|
| `avc1.*`, `avc3.*` | H.264 | `avc1` |
| `hvc1.*`, `hev1.*` | H.265 (the camera's own, before an H.264 transcode, as for a WebRTC offer) | `hvc1`; `hev1` when only it is listed |
| `mp4a.40.*` | AAC | `mp4a` |
| `opus` | Opus | `Opus` |
| `flac` | G.711 A-law and mu-law, L16 | FLAC (`fLaC`), decoded and packed (D38) |
| none of the sound names | no sound: the picture alone | |
| an empty list, or none | go2rtc's default: H.264 or H.265, and AAC | |

The answer names the stream's own codecs (`avc1.42c01f` from its SPS; go2rtc
writes fixed strings for H.265 and AAC). Then binary frames, each one whole
box run: the init segment (`ftyp`, `moov`), then a fragment (`moof`, `mdat`)
per frame; the first is the video's keyframe (the cached group of pictures of
`api.gop_cache_mib`: a viewer joining a running stream starts at once), and
sound waits for it. Each socket's media starts at zero (go2rtc's player plays
from 0 and does not seek to the first buffered time); every track moves by
the same time, so they stay in step.

- Errors, the socket kept: `mse: stream not found`; `mse: codecs not matched`
  (the list names nothing this server sends - go2rtc gives such a list its
  default - or the stream has no track the list takes); `mse: streams: <reason>`
  (no source delivered a track); `mse: already streaming on this socket` (one
  MSE at a time); `mse: too many sessions on one socket` (MSE and offers
  together, 4 in a socket's life). The player then tries its next mode.
- After the MIME type: the stream ending, or its tracks coming back with other
  codecs (one `SourceBuffer` cannot change type), is an error and the socket is
  closed `1000`; go2rtc's player starts again. Other parameter sets with the
  same codec strings (a reconnection) get a new init segment; another profile
  or level (a camera set to another resolution) changes the string the
  `SourceBuffer` was made with, and ends the socket as other codecs do.
- A reader slower than the stream falls behind its queue (2 s, 8 MiB, the live
  budget), loses frames and resumes at the next keyframe; `/api/streams` counts
  them (`dropped` of the `mse/fmp4` consumer) and the log says `mse reader fell
  behind the stream` (info, once per 10 s). One that reads nothing for 5 s is
  closed. Closing the socket ends the subscription at once.
- Measured in headless Edge 157 with go2rtc's own `stream.html?mode=mse`:
  H.265 from the camera (no FFmpeg started), H.264 with G.711 as FLAC, with
  AAC and with Opus: 4.0 s played in 4.0 s, 25 pictures a second, 1 s behind
  the live edge, as go2rtc 1.9.15 on the same streams.
- go2rtc's player keeps 5 s: at each append it removes what is older than 5 s
  before the buffered end, and a browser removes with it every picture up to
  the next keyframe. A stream whose groups of pictures last longer than 5 s
  therefore does not play in its `mse` mode, from go2rtc as from oxrtc
  (measured in headless Edge 157, F3 review: x264 720p at 4 Mbit/s with a
  keyframe every 6 s - the owner's cameras send one every 6.37 s - decoded 0 to
  19 pictures of 100 in 4 s on either server, `readyState` 1 or 2; the same
  with a keyframe every 4 s at 8 Mbit/s played 4.0 s in 4.0 s on oxrtc, a viewer
  joining it at six points of the group had its picture in 300 to 350 ms). Such
  a camera plays over WebRTC, which go2rtc's player starts beside MSE and keeps
  once it connects (its default mode).

Which browsers get what (go2rtc's player, `mode=webrtc,mse,hls,mjpeg` by default):

| Browser | MSE | What it plays from oxrtc today |
|---|---|---|
| Chrome, Edge (desktop, Android) | `MediaSource`; H.265 where the GPU decodes it (Edge 157 on this PC lists `hvc1` and `hev1`) | WebRTC, or MSE: H.265 or H.264, AAC, Opus, FLAC |
| Firefox | `MediaSource`, no H.265 | WebRTC, or MSE: H.264 (the transcode), AAC, Opus, FLAC |
| Safari 17.1 and later (macOS; iPhone and iPad from iOS 17.1) | `ManagedMediaSource` (the player uses it) | WebRTC, or MSE: H.264 or H.265 (`hvc1`), AAC, FLAC - the player drops `opus` for Safari, so an Opus-only sound is left out |
| Safari on macOS before 17.1 | `MediaSource` (FLAC from 14, AAC from 13) | WebRTC, or MSE as above |
| Safari on iPhone before iOS 17.1 | none | WebRTC, else MJPEG (its `mse` is never sent; the player's `hls` error moves it on to `mjpeg`, measured in headless Edge with `mode=hls,mjpeg`). HLS (F6) not served yet |

Safari and Firefox are read from go2rtc's player and the browsers' documented
support, not run on this PC.

What Home Assistant's library calls besides the socket, and what oxrtc
answers today (the input of F7; measured with `go2rtc-client` 0.4.0):

| Call | oxrtc 0.2.0 | go2rtc 1.9.15 |
|---|---|---|
| `GET /api` (`validate_server_version`: `>= 1.9.13` and `< 2.0.0`) | `200 {"version":"0.2.0"}`: refused, `Go2RtcVersionError` | `200` with `"version":"1.9.15"` and more fields |
| `GET /api/streams` (each stream's `producers[].url`) | `200`; a stream not running lists no producer | `200` |
| `GET /api/schemes` | `404` | `200`, a list of schemes |
| `GET /api/preload`, `PUT /api/preload?src=&video_codec_filter=&audio_codec_filter=`, `DELETE /api/preload?src=` | `200`, the codec filters ignored | `200` |
| `PUT /api/streams?name=&src=` | `200` | `200` |
| `GET /api/frame.jpeg?src=&width=` | `200`, a JPEG | `200` |
| `POST /api/webrtc?src=` with a JSON body `{"type":"offer","sdp":...}` | `400 bad offer` (oxrtc reads an SDP body only) | `200 {"type":"answer","sdp":...}` |

## MP4 over HTTP (`stream.mp4`, `frame.mp4`, M5 F4, D42)

go2rtc's `GET /api/stream.mp4` and `GET /api/frame.mp4` (its `internal/mp4`), written by the
fMP4 path MSE uses (F3): F1's init segment, then one fragment per frame from a keyframe - the
cached group of pictures' when the source runs - the media from zero, the tracks in step.
`stream.mp4` is one progressive file over a chunked body, for a `<video src>`, VLC, ffplay,
FFmpeg (and so Home Assistant's `stream` component, which reads with it), or a service that
records a few seconds (`duration`); `frame.mp4` is one keyframe with its `Content-Length`
(Safari shows none without it). Authenticated as every route.

| Query | What it changes |
|---|---|
| `src` | the stream (`404 stream not found`; absent: `400 src is required`) |
| none of the below | go2rtc's default: H.264 or H.265, and AAC |
| `mp4` (or `mp4=`) | the same set (go2rtc's "legacy") |
| `mp4=flac` | and G.711 A-law, mu-law and L16, written as FLAC |
| `mp4=all` (any other value) | and Opus (go2rtc also MP3, which oxrtc does not carry: F30) |
| `video`, `audio` (without `mp4`) | go2rtc's filters (`video`, `video=h264`, `audio=pcma`...), among what the writer takes; `video` alone has no sound, `audio` alone no picture (`audio/mp4`) |
| `duration=S` | whole seconds: the response ends before the first frame at or past `S` s of media, or `S` + 1 s after its first fragment for a stream slower than the clock; `0` is none; anything else `400 invalid duration` |
| `filename=F` | `Content-Disposition: attachment; filename="F"` (quotes, backslashes, slashes, control and non-ASCII characters replaced by `_`, 128 characters at most; RFC 6266's `filename*` beside it for a name that is not ASCII) |
| `start=cache\|next` (**oxrtc only**, D42 as amended) | where the file starts: `cache` (the default) at the cached group of pictures' keyframe, a picture at once and the file about one group behind live; `next` at the first keyframe after the request, as go2rtc starts every file (up to one group of waiting, then the live edge). On `frame.mp4`, `next` is a picture taken after the request. Anything else, or both, `400 invalid start (cache or next)`. go2rtc ignores the key |
| `queue=live\|recorder` (**oxrtc only**) | the reader's queue budget, as an RTSP reader names it: `live` (the default) drops and resumes at a keyframe, `recorder` holds more |

The sources are chosen in the order every egress shares (a camera's own H.265 before an H.264
transcode; Opus, AAC, then the PCM family). `frame.mp4` takes the picture alone, H.264 or H.265.
The `Content-Type` names the stream's own codecs (`video/mp4; codecs="avc1.42c01f,flac"`).
A failure before the head (no source opens, no codec the query takes: `codecs not matched`) is
`500` with the reason; a `frame.mp4` with no keyframe within 30 s is `500 no keyframe within 30s`.

Bounds: 64 responses at once and 16 per address, `frame.mp4` counted with them (`503 too many
mp4 responses`); the head within 60 s; a `stream.mp4` that writes nothing for 75 s ends; a
reader slower than the stream loses frames and resumes at a keyframe (`dropped` in the
listing); a stream whose tracks change ends the file there (one `moov` per file), except before its first picture: the head waits for it, so a camera whose parameter sets in band are not its SDP's (its track list published again at its first keyframe) is headed by the ones its pictures use; a client
gone, or the server's stop, ends the body at once (a complete chunked body), and a head still
waiting for its source with its subscription (the stop answers `500 the server is stopping`). The consumer is
listed as go2rtc's `mp4`.

Measured against go2rtc 1.9.15 on the same sources (2026-11-22, release build, go2rtc as the
camera: x264 720p at 25 a second with a keyframe every 2 s, A-law 16 kHz or AAC 48 kHz, and an
H.265 test source; F4's report):

| | oxrtc | go2rtc 1.9.15 |
|---|---|---|
| `Content-Type` | the stream's own strings (`avc1.42c01f`, `hvc1.1.6.L153.90`) | `avc1.` + the SDP's `profile-level-id` (`42001F`), H.265 fixed `hvc1.1.6.L153.B0` |
| H.265 sample entry | `hvc1`, as its `Content-Type` says | `hev1`, though its `Content-Type` says `hvc1` |
| `stream.mp4?duration=5` | 125 pictures, 0 to 4.96 s, in 4.0 s | 77 to 102 pictures (3 to 4 s: it waits for the next keyframe within the 5 s) |
| answer head of a running stream's `stream.mp4` | 1 to 13 ms (the cached keyframe) | 1.0 to 2.0 s (the next keyframe) |
| `frame.mp4` of a running stream | 11 ms | 1.9 s |
| sound beside the picture | starts at the keyframe's time, in step (first sound at 26 to 77 ms) | each track from 0 on its own |
| a `<video src>` of it in headless Edge 157 | plays: 1280x720, 4.0 s in 4.0 s, 100 pictures decoded in 4 s, AAC and FLAC alike | shows its first picture, then `DEMUXER_ERROR_COULD_NOT_PARSE` (AAC and FLAC alike, one run) |
| behind the live edge in that `<video>` | 2.4 s (it plays from the cached keyframe: up to one group of pictures) | - |
| `duration=5.5` | `400 invalid duration` | ignored: an endless stream |
| `src` absent | `400 src is required` | `404 stream not found` |
| Safari `User-Agent`, no `duration` | the MP4 (no HLS yet: F6) | `301` to `stream.m3u8?...&mp4` |
| Chrome `User-Agent`, no `Range`, `frame.mp4` | the file | an empty `200 video/mp4` |
| `rotate`, `scale` | not done | the init segment's matrix and `pasp` patched |
| `ffmpeg -xerror` of `duration=5`, `frame.mp4` | no error; one picture | no error; one picture |

## Motion JPEG (`stream.mjpeg`, `frame.jpeg`, the `mjpeg` message, M5 F5, D46)

go2rtc's Motion JPEG (its `internal/mjpeg`), for an `<img src>`, VLC, ffplay, FFmpeg, Home
Assistant's MJPEG camera, and go2rtc's player in `mjpeg` mode (a browser with neither WebRTC
nor MSE). Authenticated as every route, loopback included (go2rtc does not ask a loopback
client). MediaMTX 1.21.3 has no Motion JPEG output and no still.

`GET /api/stream.mjpeg?src=<stream>`: `200`, `Content-Type: multipart/x-mixed-replace;
boundary=frame`, `Cache-Control` and `Pragma: no-cache`, a chunked body of parts, each
`--frame`, `Content-Type: image/jpeg`, `Content-Length: <n>`, an empty line, the picture, a
line end - go2rtc's bytes, each part followed at once by the next delimiter (so a browser shows
it at once; go2rtc's picture waits for the next one). The pictures are the stream's own when a
source offers Motion JPEG (an MJPEG camera over RTSP or HTTP: passed on as they came, never
decoded), else the stream's one shared FFmpeg transcode (`ffmpeg:<stream>#video=mjpeg`, started
by the first reader, stopped after the idle grace by the last; go2rtc answers such a stream an
empty `200` unless the operator adds that line). A picture with no Huffman tables gets the
standard ones written in. `404 stream not found`; `400 src is required`; `500` with the reason
when no picture can be made (the head waits for the first one, within 60 s); `503 too many mjpeg
responses` past the bounds `stream.mp4` has (64 at once, 16 per address, counted together); 75 s
without a picture ends the body; a reader slower than the stream loses pictures (`dropped` in
the listing, a log line once per 10 s) and goes on at the next; `queue=live|recorder` as on
`stream.mp4`. The consumer is listed as go2rtc's `mjpeg`.

`{"type":"mjpeg"}` on `/api/ws`: answered `{"type":"mjpeg"}` once the reader is added, then one
JPEG per binary frame, the same pictures as `stream.mjpeg` (the URL's `start` is read as for
`mse`). One `mse` or `mjpeg` at a time per socket, counted with the offers. An error before the
answer (`mjpeg: stream not found`, `mjpeg: codecs not matched`) leaves the socket open; the
stream's end after it closes the socket, so that the player starts again.

`GET /api/frame.jpeg?src=<stream>`:

| Query | oxrtc | go2rtc 1.9.15 |
|---|---|---|
| `width` or `w`, `height` or `h` | scaled, a side alone keeping the shape; 1 to 7680, else `400 invalid width` / `invalid height` | the same sizes; what it cannot read ignored |
| `rotate` | `90`, `180`, `270`, `-90` clockwise, after the scale; `0` none; else `400` | the same; anything else ignored |
| `cache` | a Go duration (`10s`, `1m`, `1.5s`, `500ms`; a negative one serves nothing): a picture kept by a request that asked for a cache, served while younger, **per stream and shape**; else `400` | the same, one picture per stream served whatever the size asked |
| any of these, Motion JPEG stream | the camera's picture as it is when asked as it is (encoded again when its first segment is not a quantization table nor a JFIF, Exif or `AVI1` one, as go2rtc), encoded again otherwise | the camera's picture, all three ignored; encoded again when its first segment is not a quantization table nor a JFIF, Exif or `AVI1` one (its `FixJPEG`) |
| `hardware`, `hw` | ignored (R12) | a hardware encoder |

The answer carries `Content-Type: image/jpeg`, its `Content-Length`, `Cache-Control` and
`Pragma: no-cache`; go2rtc adds `Connection: close`.

Measured against go2rtc 1.9.15 on the same sources (2026-11-24, release build, go2rtc as the
camera: a 1280x720 test pattern at 25 a second as H.264, and as an MJPEG stream over RTSP; the
second go2rtc configured as its README asks, `ffmpeg:h264#video=mjpeg` beside the camera; F5's
report): the numbers are in D46. In short: the same headers and part layout; the first picture
of an H.264 stream in 418 ms against 3.5 s; `ffmpeg -xerror` decodes 100 pictures with no error
from both; an `<img src>` in headless Edge and go2rtc's player in `mode=mjpeg` and
`mode=hls,mjpeg` show changing 1280x720 pictures from both (the player 35 pictures in 4 s from
oxrtc, 17 from go2rtc; again in the review, in Firefox 159 and Edge, 35 and 36 against 13 and
14); the server's own CPU for 1 and 5 readers 1.2 % and 2.8 % of a core against 1.9 % and 3.3 %
(measured again in the review: 1.97 % and 4.09 % against 2.74 % and 3.71 %, oxrtc's readers
getting 25 pictures a second and go2rtc's 20 - a tie at five readers, lower per picture), one
FFmpeg for the five on both. Firefox 159 and Edge draw a webcam's pictures that carry no Huffman
tables from both servers; oxrtc writes the standard tables into them anyway (D46).

Not served: go2rtc's `POST /api/stream.mjpeg?dst=` (an MJPEG pushed into a stream: an ingest, M6),
`stream.ascii` and `stream.y4m` (M6).

## HLS and DASH (`stream.m3u8`, `stream.mpd`, M5 F6, D47)

| Request | oxrtc | go2rtc 1.9.15 | MediaMTX 1.21.3 |
|---|---|---|---|
| `GET /api/stream.m3u8?src=` | go2rtc's master playlist (`hls/playlist.m3u8?id=`, `CODECS`, `BANDWIDTH=192000`), MPEG-TS (H.264 or H.265, AAC) | the same | `/<path>/index.m3u8` |
| `...&mp4` (or `video`/`audio`) | fMP4 in the codecs the query names, as `stream.mp4` (the PCM family as FLAC with `mp4=flac`) | fMP4 | fMP4 (`hlsVariant`) |
| Segments | cut at a keyframe once 1 s long, `EXTINF` their length, 7 kept, `PROGRAM-DATE-TIME` each | two "segments" of 0.5 s of whatever was buffered | at a keyframe once 1 s, 7 kept |
| Low-Latency HLS | parts of up to 100 ms in fMP4 and MPEG-TS, `PART-HOLD-BACK` 2 parts, blocking reload, preload hint | none | fMP4 parts of 200 ms (`lowLatency` variant) |
| `GET /api/stream.mpd?src=` | a dynamic MPD over the same fMP4 segments | none | none |
| The session's routes | read with the session's 128-bit id alone | its 8-character id | the path, behind its auth |
| `{"type":"hls"}` on `/api/ws` | the master playlist of an fMP4 session in the list's codecs | the same | - |

Players, measured on the release build (F6's report, `f6-work/proof.py` and `play.mjs`: bench's synthetic camera, 1280x720 H.264 at 25 pictures a second, a keyframe a second, no sound, its frame code read off the picture in headless Edge; one pass each): hls.js 1.7.4's latency to the camera, mean and P95 - oxrtc fMP4 (Low-Latency) 506 and 525 ms, 0 pictures dropped, against go2rtc 1.9.15's fMP4 488 and 506 ms with 9 dropped and MediaMTX 1.21.3's Low-Latency 598 and 614 ms; oxrtc MPEG-TS 1542 and 1561 ms, against go2rtc's MPEG-TS which hls.js refuses (`fragParsingError`) and MediaMTX's `mpegts` variant 6715 and 6733 ms; dash.js 5.3.0 on oxrtc's DASH 2420 and 2530 ms (neither go2rtc nor MediaMTX has DASH); FFmpeg 8.0.1 reads all three (ffprobe `h264` 1280x720, `ffmpeg -xerror` decodes 8 s silently; a TS segment alone 1.000 s and 25 pictures for an `EXTINF` of 1.000). Since the F6 review, parts of 100 ms: measured again in the F6 review (the builder's `proof.py` and `play.mjs`, the same camera, four passes A B B A of the part target, 200 ms then 100 ms, mean and P95 in ms, pictures dropped): oxrtc's fMP4 at 200 ms parts 532/550 (0) and 498/517 (0), at 100 ms 292/310 (0) and 293/311 (0), against go2rtc 1.9.15's fMP4 500/522 (10), 490/506 (6), 504/528 (6), 492/510 (7) and MediaMTX 1.21.3's Low-Latency 742/760, 766/783, 759/778, 662/682; oxrtc's MPEG-TS 1971 and 1598 at 200 ms, 343 and 1370 at 100 ms (go2rtc's: `fragParsingError`; MediaMTX's `mpegts` 6669); dash.js on oxrtc's DASH 2472/2634 (24 dropped). With 6.4 s groups of pictures (the owner's cameras) FFmpeg reads all three (a TS segment alone 6.400 s and 160 pictures for an `EXTINF` of 6.4), hls.js plays oxrtc's fMP4 71 ms behind go2rtc's fMP4 and 214 ms ahead of MediaMTX's Low-Latency (relative within one run: the bench's calibration reads no frame at that group length), and its first picture comes 5.5 s after the start where go2rtc's comes at 0.4 s (oxrtc's first playlist waits for a complete segment, which a 6.4 s group makes 6.4 s long; go2rtc's pseudo-segments start anywhere). go2rtc's own player in `mode=hls` shows no picture in headless Edge from either server (Edge answers `canPlayType('application/vnd.apple.mpegurl')` "maybe" but plays no HLS natively); Safari, iOS and Chromecast are not run here.

## FFmpeg

oxrtc meets FFmpeg on two sides: as a reader of its RTSP server (openIVR's
recorder, analysis decoder and probes are FFmpeg and ffprobe), and as its own
child for the `ffmpeg:` sources and the snapshots (`ffmpeg.bin`). Three
release lines are supported (D32). Each is a job of `.github/workflows/ci.yml`
that runs the whole test suite with it on every push - the server read by
FFmpeg over TCP and with the fall-back from UDP, H.264, H.265, G.711 and
Motion JPEG, a publication with `-f rtsp`, the transcodes and their
confinement, the binary end to end:

| FFmpeg | Where it comes from | The job |
|---|---|---|
| 6.1 (`6.1.1-3ubuntu5`) | Ubuntu 24.04, `apt` | `test (ubuntu-24.04)`, on the runner |
| 7.1 (`7.1.5-0+deb13u1` on 2026-11-17) | Debian 13 "trixie", `apt`: what a Raspberry Pi 5 on Debian 13 installs | `test (ubuntu-24.04, debian:trixie)`: a `debian:trixie` container on the same runner, the image and its packages unpinned so that the job follows Debian |
| 7.1, arm64 (the same package, built for arm64) | Debian 13 "trixie" on arm64, `apt`: a Raspberry Pi 5's architecture and userland | `test (ubuntu-24.04-arm, debian:trixie)`: the same container on GitHub's arm64 runner |
| 8.0 (`8.0.1`) | Windows, Chocolatey's package | `test (windows-2025)` |

A test that does not find FFmpeg, ffprobe or go2rtc skips itself and passes on
a developer's machine; in these jobs it fails (`OXRTC_NO_SKIP`, set by
`ci.yml`), so a green job ran the suite with the FFmpeg its row names.

Three of the four jobs run on x86-64. The fourth runs the Debian 13 container
on an arm64 machine: the architecture, the system calls and the userland of a
Raspberry Pi 5 - FFmpeg's arm64 build, and `oxrtc-exec` confining it on an
arm64 kernel. It is not a Raspberry Pi: the kernel is the runner's (4 KiB
pages, where the Pi 5's has 16 KiB), and the binary it tests is built there,
not the cross-built `linux-arm64` archive of a release, which is checked by
its ELF headers (ARCHITECTURE, "Releases").

**Under an emulator (qemu-user: an arm64 image run on an x86-64 machine with
Docker) every FFmpeg oxrtc starts is refused.** Seen on 2026-11-17 with the
release workflow's `linux-arm64` archive: qemu-user serves `/proc/self/maps`
from a temporary file it writes, FFmpeg may write no file at all (its
file-size limit is zero), so it dies of `SIGXFSZ`, and oxrtc says `oxrtc-exec
refuses to start FFmpeg`; qemu-user also has no Landlock system calls. Neither
is true of a kernel. There, removing `oxrtc-exec` from beside `oxrtc` starts
FFmpeg unconfined (said at startup), which is how the rest was read under the
emulator: the relay of an RTSP camera copied, probed and decoded by FFmpeg
7.1.5 arm64, the H.264 and Opus transcodes, a snapshot.

A line older than 6.1 (Debian 12's 5.1, Ubuntu 22.04's 4.4) is neither
refused nor tested.

Read by hand as well on 2026-11-17, in containers of `ubuntu:24.04` and
`debian:trixie` (kernel 6.18) and on Windows 11, the binary relaying go2rtc's
25 pictures a second test pattern and the system's FFmpeg reading
`rtsp://127.0.0.1:<port>/cam` for 3 s: 76 packets copied (`-c copy -f
framecrc`), 76 counted by `ffprobe -count_packets`, 76 pictures decoded, 76
from the `ffmpeg:` transcode of the same stream (on Linux FFmpeg under
`oxrtc-exec`, `landlock=partial abi=7`), 76 with FFmpeg's default transport
after the `461`, and 101 packets in a 4 s copy to MP4 - the same six numbers
with 6.1.1, with 7.1.5 and with 8.0.1.

One thing differs between the lines in what oxrtc gives FFmpeg: a transcode
of a stream read by name is told not to guess the frame rate (`-fpsprobesize
0`) only by FFmpeg 8.0 and later, the first to read the relay's
`a=framerate` (D36); with 6.1 and 7.1 it guesses from 20 pictures as before,
which costs a webcam's first picture up to 20 s. The `DESCRIBE` of a reader
whose query says `source=ffmpeg:` (what the `ffmpeg:` driver, like
go2rtc's, writes for a stream read by name) is answered once the pace of
the pictures is known, 4 s at most: three gaps after the first picture a
source ever sent, at once from then on. Never later than a second before
that FFmpeg's own timeout, counted from its request, the source's start
included: the first `#timeout=N` the source says, which the relay reads in
the same `source=`, else the configuration's `ffmpeg.timeout` template (5 s
unless the file says another; a camera that took 2.5 s to answer is then
waited for 1.5 s more, one that took 4 s not at all). A relay URL written in a source
(`ffmpeg:rtsp://127.0.0.1:8554/x#video=...`) is read as go2rtc reads it,
with FFmpeg's guess.

Two things a client written against FFmpeg should know; both are FFmpeg's,
the same in front of go2rtc:

- **`frame=` of `-progress` with `-c copy`** is not printed by FFmpeg 6.1 (it
  counts encoded pictures only there; 7.1 and 8.0 count copied packets too).
  A reader that counts frames that way reads none with 6.1 from any server.
  `out_time_us` is printed by all three.
- **The first packet has no timestamp.** FFmpeg gives the first packet of an
  RTSP H.264 stream neither `pts` nor `dts`, whatever the server. oxrtc's first
  packet is a keyframe (the GOP cache), so a copy to MP4 starts with it and
  FFmpeg warns once, 6.1, 7.1 and 8.0 alike ("Timestamps are unset in a packet
  for stream 0", "Non-monotonic DTS ... changing to 1"); the recording is
  whole (101 packets in 4 s). In front of go2rtc the first packet is not a
  keyframe, the copy drops it and waits for the next one: no warning, and
  2.3 s recorded of the same 4.

## What openIVR relies on

The audit of 2026-11-16, read from openIVR's code at `c23aeee` (v2026.11.16.1),
one row per behaviour, checked against oxrtc and against the reference go2rtc
1.9.15 (`bench-bin/go2rtc-1.9.15/go2rtc.exe`, run on free ports with the
configuration openIVR writes). `go2rtc_manager.py` is `app/services/streaming/
go2rtc_manager.py`, `managed_server.py` is `app/services/ai/managed_server.py`.
Status: **ok** covered by a test that exists; **new** covered by a test added
by this audit; **fixed** a defect the audit found, with a test that failed
before; **R*n*** open, in the [ROADMAP](ROADMAP.md); **openIVR** a change on
openIVR's side of the switch (listed in the ROADMAP too).

### Process and lifecycle

| Behaviour | openIVR | oxrtc | Status |
|---|---|---|---|
| Spawned as `<bin> -config <path>`, the file written first, at every start | `go2rtc_manager.py:735` | `cli.rs` `run` tests (`-config`, `--config`, `=`, a path with a space) | ok |
| `-version` prints a line whose word after `version` is the version | `install.py:1627` (`version (\S+)`) | `cli::version_line`; `apps/oxrtc/tests/start.rs` `dash_version_prints_the_version_after_the_word_version` | new |
| Ready when `GET /api` answers 200 to the Basic credential, polled every 0.5 s, 10 s without progress | `managed_server.py:160-230`, `go2rtc_manager.py:116` | `routes.rs:132`; `e2e.rs` `wait_healthy` | ok |
| The listener on the API port is the child's own pid (a launcher stub would fail the start); on Linux `setpriv` execs oxrtc, same pid | `managed_server.py` `_listener_is_ours`, `process_guard.py:84` | one process, no launcher | ok |
| The log is on stdout, `HH:MM:SS.mmm LVL message`, the level the second word (`TRC DBG INF WRN ERR`); a line without one is relayed as a warning | `go2rtc_manager.py:72-110`, `:751` | `logging.rs`; `e2e.rs` `go2rtc_shaped` over every line | ok |
| Both pipes are drained for the life of the child; an unread pipe wedged go2rtc | `go2rtc_manager.py:751` docstring | `log_writer.rs`: a bounded, lossy queue that never blocks a worker (R25) | ok |
| A start that fails exits non-zero with the reason in its last lines (the 40 kept are quoted) | `managed_server.py` "exited immediately" | `e2e.rs` `a_port_in_use_exits_1_naming_the_address`, `no_credentials_no_start` | ok |
| Stop: `terminate()` on Windows, SIGTERM on Linux, 5 s, then kill | `managed_server.py:388-405` | `e2e.rs` `stop` (listeners and FFmpeg gone within 5 s); a Ctrl+Break to its console group, sent by a launcher child process as `run_all.py` sends one: exit 0 in a few ms with an FFmpeg held, "stopped" logged, listeners and FFmpeg gone (`ctrl_break.rs`, R14) | ok |
| The media server runs in a console group of its own (`CREATE_NEW_PROCESS_GROUP`), so the CTRL_BREAK `run_all.py` sends to the backend's group does not end it before its consumers | `go2rtc_manager.py:87-103` | `ctrl_break.rs` `ctrl_break_to_another_group_does_not_reach_it`: a break to another group ends that group's process and leaves oxrtc serving, with its FFmpeg | new |
| The child is put in a kill-on-close job object of the backend right after the spawn, and in a console group of its own: oxrtc's job for each FFmpeg is nested in that one | `process_guard.py:243`, `go2rtc_manager.py:85-102` | `killed.rs` `inside_its_parents_job_a_killed_oxrtc_still_takes_its_ffmpeg_along` (no `no job object` warning, FFmpeg gone) | new |
| A killed oxrtc takes its FFmpeg along (go2rtc's outlive it) | `run_all.py:448` kills them by name | `killed.rs` `a_killed_oxrtc_takes_its_ffmpeg_along_within_a_second` | ok |
| Each FFmpeg runs with no console and no `conhost.exe` beside it when `ffmpeg.bin` is the real binary's absolute path, as openIVR writes it (`DETACHED_PROCESS`; a bare name or a launcher keeps a hidden console: a detached launcher opens a visible one, D36); go2rtc's share its console | `go2rtc_manager.py` writes `ffmpeg.bin` | `console.rs` `ffmpeg_runs_with_no_console_host_beside_it` | new |
| Exit by itself: logged, restarted on the shared back-off, every stream registered again from the new file, holds forgotten | `managed_server.py:360-420`, `go2rtc_manager.py:1169` | openIVR's `tests/integration/test_go2rtc_process.py` against oxrtc, 4 of 4 (2026-11-15) | ok |
| Stale sweep: every process running exactly the binary's file is killed before a start and at a stop; `oxrtc-exec` is another file and never matched | `process_guard.py:50` | `oxrtc-exec` execs FFmpeg, so no stub outlives a stop | ok |

### Configuration written to `config/go2rtc.yaml`

| Behaviour | openIVR | oxrtc | Status |
|---|---|---|---|
| `api: {listen: 127.0.0.1:P, username: openivr, password: <derived>, local_auth: true}` | `go2rtc_manager.py:1840` | `apps/oxrtc/tests/openivr_files.rs` `what_openivr_writes_is_read_whole_and_without_a_warning`; `config.rs` `local_auth_true_is_what_oxrtc_does_and_false_is_not_followed` | new |
| `rtsp: {listen: 127.0.0.1:P}`, `webrtc: {listen: ":P", candidates: [<lan ip>]}` | `:1846-1862` | the same | new |
| `webrtc.ice_servers: [{urls: [stun:, turn:?transport=udp, turn:?transport=tcp], username, credential}]`, one entry, rewritten and the server restarted at each rotation (oxrtc needs no restart: `PUT /api/webrtc/ice`, **R44**, below) | `:1741-1757`, `main.py:321` | `openivr_files.rs` `the_ice_servers_openivr_writes_are_read_as_one_entry`; `app.rs` `the_ice_servers_reach_the_webrtc_server_as_openivr_writes_them`; `remote.rs`; the TCP URL's allocation by `turn_tcp.rs` (D21, **R39**: a session whose only path is the TCP-relayed one plays; both URLs allocate; a reset connection is made again; a release goes out on shutdown; refused credentials, a server that never answers and one that speaks HTTP; a 20 kB datagram from a viewer does not end the shared allocation) | ok |
| `ffmpeg: {bin: <Windows path>, h264: <template>, dedup_input: <template>}` | `:1863-1891` | `openivr_files.rs` (backslashes kept, both templates read) | new |
| `log.level` | `:1892` | `logging.rs` `levels` | ok |
| `streams:` in PyYAML flow style, sorted keys: a `_src`, a `_rec` and a public stream per camera, a `_sub` for a camera that declares a substream | `:1789-1806` | `openivr_files.rs` (five cameras, 16 streams; the server starts on the file and serves each of the 16 streams, the webcams' own sources included) | new |
| The same file after go2rtc saved it (block lists, a `preload:` section, credentials stripped): the same sources, the section warned and ignored | `go2rtc_manager.py:563-578` (why flow style) | `openivr_files.rs` `a_file_go2rtc_wrote_back_reads_the_same_streams` | new |
| The file is rewritten whole at every start, so a stream an API call added and go2rtc saved is gone: oxrtc never writes the file | `:1902-1925` | `config.rs`: "read, never written back" | ok |

### Sources of a stream

| Behaviour | openIVR | oxrtc | Status |
|---|---|---|---|
| `rtsp://user:pass@host:554/path#timeout=15`; `#timeout=` added only when the URL has no `#` | `go2rtc_manager.py:217-225` | `crates/rtsp-client/tests/fake_camera.rs`, `refused_credentials_are_auth_and_travel_decoded_outside_the_url` | ok |
| `http(s)://` camera (an MJPEG webcam: three of the owner's six cameras, two plain `http`, one `https`) as the only source of `camera_N_src`, with no `#` option | `camera_url.py:30`, `go2rtc_manager.py:1789`, `:217-225` | `crates/http-client` (`tests/webcam.rs`: stream, picture per request, Basic, Digest, redirects, stalls, the bounds; `tests/https.rs`; `multipart_tests.rs`); `crates/rtsp-server/tests/jpeg.rs` (FFmpeg decodes the RTP/JPEG to the same picture as the file, 4:2:0, 4:2:2, restart intervals, and with Huffman tables of the picture's own: FFmpeg's `-huffman optimal` and libjpeg's `optimize_coding`); `crates/rtsp-server/src/recode_tests.rs` (the scan coded again, symbol for symbol, on real files and on random blocks); `crates/http-client/src/connect_tests.rs` (an address that never answers does not use up the time of the next); `apps/oxrtc/tests/webcam.rs` (the binary, in openIVR's layout: `camera_N_src?video` read by ffprobe as `mjpeg,640,360`, the recorder's re-encode, `/api/streams` listing `video, recvonly, JPEG`, `frame.jpeg`, a WHEP viewer through the H.264 transcode, the recorder's hold refused for sound then taken on the video, no password in the log or the listing) | ok (R1, D19, R41); pictures RTP/JPEG cannot describe (progressive, 4:4:4, sides over 2040 pixels) **R42** |
| `rtsps://` camera, which the TLS policy `preferred` rewrites a camera to when it answers in TLS (the probe checks no certificate, the URL carries no option) | `tls_policy.py:235-300`, `go2rtc_manager.py:895-908` | `crates/rtsp-client/tests/rtsps.rs`: a camera with a self-signed certificate plays with the bare URL (the request line names `rtsps://`, no password in any request, media over TCP interleaved) and is torn down over TLS; `#verify` refuses it with the reason in the error and no RTSP request sent, accepts a certificate the configuration trusts for the host, refuses a trusted one made for another host; a port that speaks plain RTSP, a handshake that never ends and a camera that drops the connection are each named; the password of a camera that refuses it travels in the `Authorization` header only; the flags are checked at creation; `crates/net/src/tls_tests.rs`: a 1024-bit RSA signature (rustls refuses to verify it) and a certificate that is not DER are taken without `#verify` | ok (R8, D20); no key in the file for the extra roots or for a default of `#verify` yet |
| `rtsp://127.0.0.1:<own port>/camera_N_src?video#timeout=20`, and `…/camera_N_rec?audio=opus`: relays between streams | `:228-249`, `:402-428` | aliases (D14): `crates/core/tests/relay.rs`, `aliases.rs`; `start.rs` `a_loopback_url_on_our_own_port_is_read_in_process` | ok |
| `ffmpeg:camera_N_src#video=h264#width=1920#height=1080#timeout=20` with the `h264` template of the file. It feeds WebRTC viewers, so it is a live reader of `camera_N_src` (`&queue=live` in the URL the driver reads, 2 s) where go2rtc's lags up to 10 s on a loaded CPU, and, with FFmpeg 8.0 or later, it reads the relay with `-fpsprobesize 0` (no 20-picture frame-rate guess; a slow webcam's rate is in the relay's `a=framerate`, which FFmpeg reads since 8.0, D36: a webcam's transcode starts in 0.1-0.3 s where it took 0.75 s at 30 pictures a second and 20 s at one, with 28 MiB less, the output's rate as before): the two differences from go2rtc's own line, which `args_tests.rs` states | `:420-428` | `crates/ffmpeg/tests/transcode.rs` `a_stream_by_name_is_transcoded_to_h264_and_scaled`; `openivr_layout.rs`; `args_tests.rs` `a_video_transcode_of_a_stream_reads_its_input_as_a_live_reader`; `queue_class.rs` `a_video_transcode_that_falls_behind_is_cut_to_a_keyframe` (real FFmpeg, 6 s without reading, listed `live`, cut); `paused_reader.rs` `a_video_transcodes_input_is_cut_to_a_keyframe_after_5_s` | ok (R23b; not measured on a loaded Pi, phase D) |
| `ffmpeg:rtsp://127.0.0.1:P/camera_N_src#audio=opus#timeout=20`: the plain sound transcode, both tracks in, Opus out. openIVR records its output (`camera_N_rec`), so it stays a recorder reader (10 s): a sound cut to a keyframe is a hole in a file | `:395-399` | `transcode.rs` `pcma_from_an_rtsp_url_is_transcoded_to_opus`; `openivr_layout.rs`; `args_tests.rs` `a_transcode_with_sound_keeps_the_recorder_class`, `a_url_input_is_left_as_it_is`; `queue_class.rs` `a_sound_transcode_keeps_the_recorder_class` | ok |
| `ffmpeg:http://127.0.0.1:<port>/camera_N_src#audio=opus#input=dedup_input`: an endless WAV of G.711 at 16 kHz over HTTP/1.0, through the named input template; a `415` makes the run fail | `audio_dedup.py:228-247, 470-490`, `:1890` | `transcode.rs` `the_deduplicated_sound_served_over_http_is_transcoded_with_openivrs_input_template`, `a_deduplicator_that_answers_415_fails_the_run` | new |
| A stream's sources are tried in order and a failing one falls through to the next (the de-duplicated sound, then the plain one; the picture does not wait for the sound, nor for a sound its input declined that is waiting out its backoff: a webcam's viewer was answered after up to 8 s, D36) | `:362-399` | `crates/core/tests/fallback.rs` (`a_viewer_that_comes_while_a_declined_sound_waits_out_its_backoff_is_answered_at_once`) | ok |
| `camera_N_sub`: one source, never an `ffmpeg:` fallback | `:922-958` | as `_src` | ok |

### HTTP API

| Call | openIVR | oxrtc | Status |
|---|---|---|---|
| `PUT /api/streams?name=N&src=A&src=B`, 200 or 201 read as success; a `400` whose body holds `yaml:` makes openIVR rewrite the file and restart (no oxrtc answer says `yaml:`) | `:2094`, `:2156` | `routes.rs:147`; `routes.rs` `streams_put_get_delete` | ok |
| `DELETE /api/streams?src=N` for the stream and its `_sub`, `_rec`, `_src`; only the main one's `200` or `204` is read | `:979-993` | `routes.rs:168` | ok |
| `GET /api/streams`: `producers[].medias` strings (`"audio, recvonly, PCMA/16000"`, direction seen from the server), `producers[].receivers[].codec.codec_type` and `.packets` (omitted at 0), `consumers[].format_name == "preload"`, `consumers[].medias` (`"audio, sendonly, OPUS"`) | `:252-355` | `info.rs`; `routes.rs` `streams_json_shape` | ok |
| `PUT /api/preload?src=camera_N_rec&audio=opus` and `&video=` (an empty value): `200` once the tracks are known, `500` with a reason when a wanted media cannot be produced, waited for up to 90 s; one at a time from openIVR, but oxrtc takes them in parallel | `:1328`, `:189` | `preload.rs` (every case) | ok |
| A preload of a stream that does not exist: go2rtc answers `500 streams: stream not found: N`. A `404` would be read as "no preload in this server" and every hold given up until the next restart (`_holds_unsupported`) | `:1089-1098` | `routes.rs:184`; `preload.rs` `a_preload_of_an_unknown_stream_is_a_500_not_a_404` | fixed |
| A second preload replaces the first once it holds; a failed one leaves the first | `:1119-1167` | `preload.rs` `a_new_preload_holds_before_the_old_one_is_released`, `a_replacement_that_fails_leaves_the_hold` | ok |
| `DELETE /api/preload?src=camera_N_rec`, the answer ignored | `:1548` | `routes.rs:213` | ok |
| `GET /api/frame.jpeg?src=camera_N_sub` (or `_src`)`&width=320`, `200 image/*` or nothing, 4 s, at most two at a time | `:1632-1647` | `routes.rs` `frame_jpeg_route`; `stills.rs`; `e2e.rs` `check_still`. A cold camera inside the 4 s is **R35** | ok |
| `POST /api/webrtc?src=camera_N`, `Content-Type: application/sdp`, the browser's offer (IPv6 candidates already filtered), `201`, the answer as `application/sdp` with an IPv4 host candidate | `:1662`, `streaming.ts:427-460` | `routes.rs` `webrtc_route`; `crates/webrtc/tests/whep.rs`; `e2e.rs` `check_whep` | ok |
| The answer names ONE stream for the picture and the sound (`a=msid`, `a=ssrc ... msid:`, `a=msid-semantic`), as go2rtc's does: a player that sets `srcObject = event.streams[0]` at each `track` event keeps its picture. openIVR's player took `streams[0]` until 2026-11-17 and was black with oxrtc 0.1.x; it now builds a stream of its own from the tracks, and plays either way | `streaming.ts` (`ontrack`) | `crates/webrtc/src/msid.rs`; `crates/webrtc/tests/one_stream.rs`; in a real browser, `bench/stream_check.py` (headless Edge) | fixed (W1, D30) |
| The offer's `User-Agent: openIVR-whep/<id>` is the consumer's `user_agent` in `GET /api/streams`; the consumer is `webrtc/whep` | `:1664`, `whep_sessions.py:19` | `routes.rs` `webrtc_route` and `streams_json_shape`; `whep.rs` | new |
| A `404` whose body says `stream not found` makes openIVR register that camera and retry once; a `500` whose body holds `streams:` is "camera unreachable" (503), any other failure a `502` | `:1701-1714` | `routes.rs` `webrtc_route` (the `404` and the `streams: …` `500`) | ok |
| An offer for a camera whose transcode is slow to start is answered, not refused: the owner's `camera_4` (an HTTPS Axis webcam, one picture a second) is H.264 only through `ffmpeg:camera_4_src#video=h264`, FFmpeg reads 20 of its pictures before it opens the input, and go2rtc answered after 28.6 s. openIVR waits 30 s for the answer (`timeout=30.0`), and a `500` is read as "camera unreachable" | `go2rtc_manager.py:1685-1760` (`whep_offer`, `_locked_whep_post`) | the answer waits 29 s, the subscription 28 s while a source is still starting and none has failed (`first_track_timeout`), the FFmpeg driver gives its transcode 30 s to its first frame (D27); a camera that refuses the connection is still answered at 8 s, one that does not answer at all when its attempt fails (16 s measured). Measured on the real camera 2026-11-16: answer 20.7 s, first picture a keyframe; `crates/core/tests/slow_start.rs`, `apps/oxrtc/tests/cold_transcode.rs`, `apps/oxrtc/tests/real_cameras.rs` (ignored: the owner's cameras) | fixed |
| A viewer's consumer goes when the browser closes its peer connection: go2rtc has no call to remove one, and openIVR relies on that (the `DELETE` of its own resource only forgets the session). oxrtc also has the call (R45): the `Location` of its answer takes a `DELETE` that frees the consumer at once, and a `PATCH` for trickled candidates, so openIVR's own `DELETE`/`PATCH` can forward them (below) | `whep_sessions.py:3-20`, `streaming.ts:536-560` | `whep.rs` (`client.stop(Control::Close)`: session and consumer gone); `crates/webrtc/tests/resource.rs`; `crates/api/tests/whep_resource.rs`; `e2e.rs` `check_session_resource` | ok |
| The session resource (R45, D23): `Location: /api/webrtc/<id>`, `PATCH` with `application/trickle-ice-sdpfrag` `204`, `DELETE` `200`, an unknown id `404`; an answer with the body and every header go2rtc clients read (`201`, `application/sdp`); a client that ignores `Location` plays as before | `whep_sessions.py`, `whep.py` (Q1-07) | `crates/api/tests/whep_resource.rs` (headers, media type, bounds, statuses, authentication, malformed ids stopped at the API); `crates/webrtc/tests/resource.rs` (an id per answer, 128 bits; a client whose candidates come by `PATCH` alone plays; a trickled candidate is checked by a full-ICE server and gets its TURN permission; a `DELETE` leaves no consumer and no session when it returns, the viewer is told, a second one is `404`; a session that ended by itself is `404`; unknown ids; ICE restart `501`; the bounds; a `DELETE` gives the caller's place back); `e2e.rs` `check_session_resource` (the binary over HTTP) | ok |
| The Basic credential on every call, the loopback ones included; none is `401` | `:744`, `:1840` | `routes.rs` `every_route_requires_authentication` | ok |
| The browser with no usable candidate in its offer (IPv6 filtered, mDNS names): the server never starts checks, and learns the viewer's address from its own | `streaming.ts:427` | ICE-lite, R27 `a_binding_request_reaches_its_session_and_teaches_it_the_route`; `candidates.rs:221` (an mDNS name is read and skipped); every test of `whep.rs` | ok |

### RTSP readers (`rtsp.listen`, TCP interleaved)

| Reader | openIVR | oxrtc | Status |
|---|---|---|---|
| The recorder's segmenter: FFmpeg, `-rtsp_transport tcp -timeout 20 s -fflags +genpts+discardcorrupt -i rtsp://127.0.0.1:P/camera_N_rec?video=copy&audio=opus` (or `?video=copy` for a camera held by its picture), `-map 0:v:0 -map 0:a:0?`; it must lose nothing when paused for seconds | `segmenter.py:405-436`, `:1959-1987` | `crates/rtsp-server/tests/ffmpeg.rs`; `paused_reader.rs` `a_reader_paused_5_s_loses_nothing`; `protocol.rs` `queries_select_the_described_media` | ok |
| The analysis decoder (OpenCV's FFmpeg and the Go sidecar's FFmpeg): `-rtsp_transport tcp -i rtsp://127.0.0.1:P/camera_N_sub?video&queue=live`, or `camera_N?video&queue=live` when no substream is read; the sidecar's FFmpeg (and the `ffprobe` that precedes it) reads `camera_N?video&queue=live` too (before R36: no query, and no class). It shows what it reads, so it wants the live budget (2 s), not the recorder's (10 s) | `go2rtc_manager.py:1989-2042`, `frame_capture.py:245-275`, `sidecar/internal/source/stream.go:330-390` | `queue=live` (D18): `paused_reader.rs` `a_reader_that_asks_for_the_live_queue_is_cut_to_a_keyframe`. openIVR says `?video&queue=live` since `v2026.11.16.4` (`analysis_url()`, and `capture_url()` for the sidecar); FFmpeg and OpenCV send the query in `DESCRIBE` (measured). Reviewed 2026-11-16 with the real readers on the binary: `apps/oxrtc/tests/openivr_analysis.rs` (real FFmpeg and ffprobe on both URLs: the picture alone, listed as `queue: live` while it reads, `recorder` without the key, and the reference go2rtc 1.9.15 answering the same URL), `openivr_readers.rs` `the_analysis_decoders_url_is_the_picture_alone_read_as_a_live_reader`, `ffmpeg.rs` `ffmpeg_reads_the_analysis_url_as_a_live_reader` | ok (R36) |
| The sound de-duplicator: its own RTSP session (`audio_dedup.py`): the picture described alone on a connection kept open, the plain URL on a second, the sound alone set up on the channel pair twice its section's index, `PLAY` on the URL without the trailing slash, `OPTIONS` in-band every 20 s, `User-Agent: openIVR-audio` | `audio_dedup.py:140-460` | `crates/rtsp-server/tests/openivr_readers.rs` `the_sound_deduplicators_session_carries_the_sound_alone`, every step replayed | new |
| No query: every track the stream can give, the picture first (the capture sidecar's `camera_N`); a query naming one media excludes the other. **Differs from go2rtc on openIVR's layout** (measured 2026-11-16, 1.9.15, the three streams openIVR writes for one camera with sound): `rtsp://…/camera_N` and `…/camera_N?video&audio` answer the picture alone, the sound being started only by a consumer that names its codec (`audio=opus`, a browser's offer); oxrtc answers both. The sidecar's FFmpeg drops the sound (`-an`), so the cost is the sound transcode it starts when nothing holds `camera_N_rec` with sound, and on a camera with no sound a DESCRIBE that waits for the sound's walk (measured cold: 7.7 s in oxrtc, 12.0 s in go2rtc, so not a regression). The sidecar's URL says `?video&queue=live` since openIVR `v2026.11.16.4` (R36), so the sound is no longer asked for | `go2rtc_manager.py` `capture_url()`; `capture_sidecar.py`, `main.py` (`_start_capture_sidecar`) | `protocol.rs` `queries_select_the_described_media` | ok (R36) |
| A camera restarted or a sound transcode restarted under a steady picture does not move a long-lived reader's clock; sender reports dated by arrival | `segmenter.py:370-395` | `crates/rtsp-server/tests/timeline.rs` (D12) | ok |
| A reader asking for a source that is still cold waits for its tracks; the first reader decides nothing about the order | `go2rtc_manager.py:228-249` | `crates/core/tests/fallback.rs` `the_first_track_list_arrives_within_the_deadline_of_the_subscribe_call`; tracks always video first | ok |

### RTSP credentials (R28): what each openIVR reader would need

openIVR sets no `rtsp.username` (`go2rtc_manager.py:1880`: the `rtsp:` section
holds `listen: 127.0.0.1:P` only), so with the file it writes today **nothing changes**
and every reader below works with the URL it builds. If the owner sets a pair
(by hand, or later from the settings), the readers must carry it; none does
today, because go2rtc never asked of loopback. In oxrtc's terms (checked: the
server's tests read each shape with real FFmpeg and the two raw sessions):

| Reader | URL built at | Carries a credential? | With `rtsp.username` set |
|---|---|---|---|
| Recorder segmenter (FFmpeg) | `go2rtc_manager.py:2010` `recording_url` | no | `rtsp://user:pass@127.0.0.1:P/camera_N_rec?...`; FFmpeg answers the challenge itself (Digest) |
| Analysis decoder, OpenCV and the sidecar's FFmpeg and `ffprobe` | `:2062` `analysis_url`, `:1976` `relay_url`, `:2067` `capture_url` | no | the same, userinfo in the URL; the sidecar passes `RelayURL` whole to FFmpeg and to `ffprobe` (`sidecar/internal/source/stream.go:344`, `:415`), so it needs no change, but its logs and OpenCV's must not print the URL |
| The sound de-duplicator's raw session (`audio_dedup.py:506`, `_connect`) | its own sockets, no FFmpeg | no | it must send `Authorization: Basic <base64(user:pass)>` on every request (oxrtc accepts Basic from the first request, no challenge needed; a `401` ends the session) |
| `ffmpeg:` sources inside the media server (`go2rtc_manager.py:400`, `:259`, `:436`) | written in the stream sources | n/a | nothing: oxrtc gives each run its own credential (above), and the plain `rtsp://127.0.0.1:P/...` sources are in-process |
| `rtsp.local_auth: false` | `rtsp:` section of the file | n/a | go2rtc's rule: loopback readers need nothing, everyone else the pair |

The lowest-risk way to protect a machine whose RTSP port is reachable from the
LAN, with no change to any openIVR reader: `rtsp.username`/`rtsp.password` with
`rtsp.local_auth: false`. The way that also covers a local process (SSRF through
the backend, another account): the pair, `local_auth` left at `true`, and the
three URL builders above adding the userinfo (one helper, so the password is
written once and redacted by `app/core/redact.py` like a camera's).

### openIVR's side of the switch (changes in openIVR, not in oxrtc)

| Behaviour | openIVR | What must change | Status |
|---|---|---|---|
| openIVR's `DELETE`/`PATCH` of its own session (`whep_sessions.py`, `whep.py`, Q1-07) cannot free or feed go2rtc's consumer, which goes only when the peer connection closes | `whep.py:124` (`Location`), `go2rtc_manager.py:1672` (`whep_offer`, which returns the answer only) | with oxrtc, `whep_offer` keeps the `Location` of the media server's `201` (a path, `/api/webrtc/<id>`) beside its own session id; openIVR's `DELETE` sends `DELETE` there (a `404` is "already closed"; any other failure is logged and the session forgotten as today), its `PATCH` forwards the `application/trickle-ice-sdpfrag` body (`204`; `501` is an ICE restart, which the live view treats as a reconnect). With go2rtc there is no `Location` and nothing changes. The `User-Agent` mapping stays as the fallback. The id is a capability: not logged | oxrtc side done (R45); **openIVR** not yet |
| A rotation of the TURN credentials restarts the media server: `set_ice_servers` returns `True` for a pair that differs, then `restart()`, which cuts every recording and every live view for a few seconds, at most once per credential lifetime (24 h by default) | `main.py:321` (`_apply_turn_credentials`), `go2rtc_manager.py:1764` (`set_ice_servers`), `:817` (`restart`) | with oxrtc, after `set_ice_servers` returned `True`, `PUT /api/webrtc/ice` with the entry it just remembered (`ice_servers()`, a JSON list, Basic credential as every call) instead of `restart()`; an answer other than `200` (go2rtc's `404`, a failed call) falls back to the restart, which is correct for both servers. The file is still rewritten at the next start from the same `_ice_servers`. oxrtc side done (R44, D22): the call, the retirement of the old allocation, the tests in `crates/webrtc/tests/remote.rs` (`new_credentials_serve_new_sessions_and_the_old_session_keeps_playing`: a session made before keeps playing, one made after is allocated with the new user name; `the_old_allocation_is_released_once_its_last_session_ends`; `refused_new_credentials_leave_the_old_allocation_offered`; `retired_allocations_are_bounded`), `crates/api/tests/ice.rs`, `apps/oxrtc/tests/ice_update.rs` | oxrtc done; **openIVR** the call in `_apply_turn_credentials` (R44) |
| The running version is compared with `deps/GO2RTC_VERSION` and a warning says to run `install.py` to replace `bin/go2rtc`; the system page shows the version as `go2rtc` | `go2rtc_manager.py:767-787`, `system.py:159-169` | oxrtc reports `0.1.0`: the check must name the server it expects (a spurious warning at every start otherwise). Done in openIVR `v2026.11.16.5` (`c0dcb7c`): the program is told by its binary's name (`app/services/streaming/media_server.py`: `oxrtc*` is oxrtc), each is compared with its own pin (`deps/GO2RTC_VERSION`; `deps/OXRTC_VERSION` is `0.1.0` since R38, and with no pin file nothing is warned), the warning names the server, and the system page row keeps the id `go2rtc` with the running server's name and version. Still naming go2rtc, listed in openIVR's ROADMAP: the live view's subtitle and `updates.warnGo2rtc` (14 languages) and the licence page's component row | ok (R37) |
| `install.py` downloads go2rtc by a pinned version and checks its SHA-256 (`deps/GO2RTC_SHA256SUMS`); `Dockerfile` does the same | `install.py:1525-1700`, `Dockerfile:97-123` | an oxrtc archive per platform (`oxrtc` and, on Linux, `oxrtc-exec` beside it) with its own pin; a release of a private repository cannot be downloaded without a token. Done in openIVR `v2026.11.16.7` (`522da7d`): `install_oxrtc()` (a step after go2rtc, optional, never fatal) reads `deps/OXRTC_VERSION` (`0.1.0`), fetches `oxrtc-v<version>-<windows-amd64.zip, linux-amd64.tar.gz, linux-arm64.tar.gz>` from `$OXRTC_RELEASE_SOURCE` (a directory or an `https://` folder) or the line of `deps/OXRTC_SOURCE`, deletes an archive whose SHA-256 is not the one in `deps/OXRTC_SHA256SUMS`, installs nothing from a Linux archive that lacks `oxrtc-exec`, and renames `oxrtc` and `oxrtc-exec` into `bin/` together (the notices go to `bin/oxrtc-licenses/`); the version is read by go2rtc's own reader (`oxrtc version 0.1.0 windows/amd64`); `python -m app.scripts.pin_oxrtc_release <release directory>` writes the two pin files from a `release_local.py` output and refuses to re-pin a version to other bytes. Run end to end here on the three real archives (Windows: `bin/oxrtc.exe -version` prints `oxrtc version 0.1.0 windows/amd64`; Linux: both executables placed, not run). Still open: **where the archives are published** (owner question 2: `deps/OXRTC_SOURCE` is empty, so a fresh clone installs nothing and keeps go2rtc until it is filled; the pinned archives are `oxrtc-release/v0.1.0/` beside the repositories, built from `489b3b2`), and the `Dockerfile` stage (no Docker here) | done, source open (R38) |
| `run_all.py` kills stale processes by image name on Windows (`ffmpeg.exe`, `go2rtc.exe`) and by project ownership elsewhere | `run_all.py:405-460, 985-995` | add `oxrtc.exe` (and `oxrtc-exec`) to both. Done in openIVR `v2026.11.16.5`: `taskkill /IM oxrtc.exe` beside `ffmpeg.exe` and `go2rtc.exe` (`WINDOWS_STALE_IMAGES`, `kill_windows_images()`), and `is_stale_project_process` recognises a process whose image starts with `go2rtc` or `oxrtc` (`oxrtc.exe`, `oxrtc-exec`) when it belongs to the checkout; pinned to the backend's list by `test_the_list_agrees_with_the_one_the_backend_reads`. The taskkill is by image name, as go2rtc's was, so it also reaches an oxrtc of another checkout or of a bench | ok (R37) |
| `/health` reports `checks.go2rtc`, and the updater warns when it is `false` | `main.py:1363`, `updater/runner.py:69` | a key named after go2rtc: a rename is a change of both sides, otherwise nothing | ok |
| The first run of a new executable on Windows: Windows Defender Firewall asks, per program path, whether `oxrtc.exe` may listen (the rule go2rtc has is `go2rtc.exe`'s, at its path; the owner's machine holds one such rule per copy that ever listened). An unanswered prompt on a machine that starts the NVR at logon leaves the WebRTC port (`webrtc.listen`, TCP and UDP) closed to every other device on the LAN, while a browser on the same machine still plays: the phone and the other PCs lose the live view and nothing in either log says why | `go2rtc_manager.py:1846` (`webrtc.listen`), `install.py` (no firewall step) | nothing in oxrtc can answer it: a rule is the installer's (`netsh advfirewall firewall add rule name="oxrtc" dir=in action=allow program=<bin\oxrtc.exe> enable=yes profile=private,public`, administrator) or the operator's click | **R40** |
