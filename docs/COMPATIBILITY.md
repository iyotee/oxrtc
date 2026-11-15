# Compatibility with go2rtc

The surface openIVR uses, read from its source (`app/services/streaming/
go2rtc_manager.py`, `audio_dedup.py`, `app/services/recording/`,
`app/services/ai/frame_capture.py`) on 2026-11-14, against go2rtc 1.9.15. This
is the contract of the compatibility layer: everything here must behave as
described, and anything not here is out of scope until a consumer needs it.

## Process

- Launched as `oxrtc -config <path>` (go2rtc's flag; `--config` too).
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

Two go2rtc keys about protection are not merely ignored. `api.local_auth: true`,
which openIVR writes, is what oxrtc does anyway (loopback callers authenticate
too, D7); `false` is not followed, and warned about. A non-empty
`rtsp.username` or `rtsp.password` **refuses the file** (exit 1): oxrtc has no
RTSP authentication yet (ROADMAP R28) and will not serve open a server the
file protects. openIVR writes neither: its RTSP server listens on loopback.

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
| `webrtc.max_sessions` | 64 | WHEP sessions at once, every caller together (D9) |
| `webrtc.max_sessions_per_ip` | 32 | WHEP sessions one caller address holds (D9) |
| `ffmpeg.max_processes` | 64 | FFmpeg processes at once, stills included; go2rtc reads it as a template nothing names |
| `ffmpeg.max_memory_mib` | 4096 | address space of each FFmpeg (`RLIMIT_AS`), Linux only, where FFmpeg runs confined through `oxrtc-exec` (ARCHITECTURE "Processes"); go2rtc reads it as a template nothing names |

The limits are whole numbers of at least 1; a per-address one is effective up
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
closes.

| Call | Answer | Notes |
|---|---|---|
| `GET /api` | `200 {"version": "..."}` | health and version |
| `GET /api/streams` | `200 {name: StreamInfo}` | `?src=name` for one stream (`404` if absent) |
| `PUT /api/streams?name=N&src=A&src=B` | `200` | create or replace; same sources = no-op; `400` with a reason for a source no driver accepts |
| `DELETE /api/streams?src=N` | `200`, `404` | openIVR treats `404` as normal |
| `PUT /api/preload?src=N[&video=…][&audio=…]` | `200` held, `500` with a reason | see below |
| `DELETE /api/preload?src=N` | `200` | releases |
| `GET /api/preload` | `200 {name: {...}}` | listing |
| `POST /api/webrtc?src=N` | `201` (or `200`) `application/sdp` answer | body: the SDP offer; `404 stream not found`; `500 streams: …` when no source opens |
| `GET /api/frame.jpeg?src=N&width=W` | `200 image/jpeg` | anything else is a failure to openIVR |

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
- `consumers[].format_name` is `preload` for a preload, `webrtc`, `rtsp`, `mse`
  otherwise. openIVR decides whether a camera is held by looking for `preload`.
- oxrtc only, ignored by go2rtc clients: `consumers[]` also carry `frames`,
  `bytes` and `dropped` (lost because the consumer fell behind its queue's
  budget: 10 s or 32 MiB for an RTSP reader, 2 s or 8 MiB otherwise), and each stream a
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
stream replaces the first preload. go2rtc serialises every preload behind one
global mutex; oxrtc must not (a failing camera must not delay the others).

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
- `ANNOUNCE`/`RECORD` (publication) for transcoders only, on one-time path
  tokens; never open to arbitrary clients.
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
| `rtsp://…`, `rtsps://…` | a camera, over TCP; `#timeout=N` = stall timeout in seconds |
| `rtsp://127.0.0.1:<own port>/<name>?<query>` | another stream of this server: becomes `stream:<name>?<query>`, no network hop; its `#timeout=N` is accepted and has no effect |
| `stream:<name>?<query>` | an alias of stream `<name>`, passing what `<query>` asks (`video`, `audio=opus`...): its consumers read that stream's own source, resolved when they subscribe (D14). A stream that would read itself, directly or through other streams, is refused (`400`, `a stream cannot read itself through b -> a`) |
| `ffmpeg:<stream name>#video=h264#width=W#height=H` | transcode of a stream of this server; declares `video/h264` |
| `ffmpeg:<url>#audio=opus#input=<template>#timeout=N` | transcode of a URL (rtsp, http) with a named input template; declares `audio/opus` |
| `http(s)://…` (MJPEG) | a webcam; roadmap R1 |

`exec:`, `echo:`, `expr:` and every scheme not listed are refused with `400`.

The `ffmpeg:` templates are go2rtc's, with one difference (D11): the `rtsp`
input template has no `-fflags nobuffer`, which made every transcode wait for
the camera's next keyframe. A file that names its own `rtsp` template keeps it.

## WebRTC

- At most 64 sessions at once (`webrtc.max_sessions`) and 32 for one caller
  address (`webrtc.max_sessions_per_ip`, the HTTP peer of `POST /api/webrtc`);
  beyond either the offer is answered `503`.
- WHEP over `POST /api/webrtc`. The answer is complete (no trickle): host
  candidates for every local address plus `webrtc.candidates`, on the single
  UDP port of `webrtc.listen`; ICE-TCP passive candidates (`tcptype passive`)
  at the same addresses on a TCP listener at that port, as go2rtc listens on
  both (a TCP port that cannot be bound is warned about, and only TCP is
  lost); and, from `webrtc.ice_servers`, a server-reflexive candidate per
  `stun:` server and a relayed one per `turn:` server.
- `webrtc.ice_servers` as openIVR writes it (`set_ice_servers`: one entry,
  its relay's `stun:` and `turn:` URLs, a `username` and `credential` that
  rotate, the media server restarted with each new pair): `stun:host[:port]`
  and `turn:host[:port][?transport=udp]` are used (port 3478 by default, the
  name resolved at each allocation); `turn:...?transport=tcp`, `turns:` and
  `stuns:` are logged at startup and not used (D13); a malformed URL is warned
  about by its position, never printed. A `turn:` URL without a username and
  credential is not used. At most four TURN and four STUN servers. The TURN
  allocation is made at startup and kept; an offer answered in the first 2 s
  after startup waits for it, until then at most. A relayed or reflexive candidate makes the session full ICE
  instead of ICE-lite. Neither the user name nor the credential is ever
  printed.
- The offer's codecs choose the source: a browser offering H.265 gets the
  camera's own stream; one offering only H.264 gets the transcode. Audio: Opus
  (PCMA/PCMU when offered and the source has them).
- IPv6 candidates: openIVR strips them from both offer and answer; emit IPv4
  first.
- RTP time is the hub's timeline, as for RTSP: a source's restart moves a
  viewer's clock by the time the source was away, picture and sound alike.
- A viewer's keyframe request (PLI, FIR) is answered from the cached group of
  pictures, at most once a second per session, the frames re-stamped after
  the last ones sent (ARCHITECTURE "Keyframe requests"); go2rtc has no answer
  to it for an RTSP camera, whose next keyframe the viewer waits for. A
  session that connects more than 2 s after its offer resumes from the same
  cache rather than at the camera's next keyframe.
