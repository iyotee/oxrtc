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
| `api.max_streams` | 512 | streams at once, from the file and `PUT /api/streams` together (D16) |
| `api.max_consumers_per_stream` | 64 | viewers, readers and preloads of one stream; one more is refused (`503` to an offer or a `DESCRIBE`) (D16) |
| `api.max_frame_mib` | 8 | largest frame a source may publish, in MiB; a larger one is refused and counted in the source's `rejected` (D16) |
| `api.gop_cache_mib` | 8 | the current group of pictures kept per source, in MiB: what a new consumer starts from and what a viewer's keyframe request is answered with. A group larger than it is not cached, and said once per source (`group of pictures larger than the keyframe cache`, with its size): 8 MiB is a 6 s group at about 10.6 Mbit/s (D16) |
| `rtsp.queue_max_mib` | 32 | what one RTSP reader's queue holds, in MiB (a recorder, D15) |
| `rtsp.queue_max_seconds` | 10 | how long its oldest frame may wait; beyond either, it resumes at a keyframe |
| `webrtc.queue_max_mib` | 8 | the same for a live consumer: a WebRTC viewer, and the API's stills and preloads, of the same class |
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
closes.

| Call | Answer | Notes |
|---|---|---|
| `GET /api` | `200 {"version": "..."}` | health and version |
| `GET /api/streams` | `200 {name: StreamInfo}` | `?src=name` for one stream (`404` if absent) |
| `PUT /api/streams?name=N&src=A&src=B` | `200` | create or replace; same sources = no-op; `400` with a reason for a source no driver accepts |
| `DELETE /api/streams?src=N` | `200`, `404` | openIVR treats a failed delete of `_sub`, `_rec` and `_src` as normal. go2rtc answers `400 yaml: path not exist` for a name never defined and `200` for one deleted before; openIVR reads only "200 or 204" of the main stream's |
| `PUT /api/preload?src=N[&video=…][&audio=…]` | `200` held, `500` with a reason | see below. **An unknown stream is `500 streams: stream not found: N`, never `404`**: openIVR reads a `404` of a preload as "this go2rtc has no preload" and holds nothing until it restarts |
| `DELETE /api/preload?src=N` | `200` | releases; a stream held by nothing is `200` too (go2rtc: `500 streams: preload not found`; openIVR ignores the answer) |
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
- `consumers[].format_name` is `preload` for a preload, `webrtc/whep` for a
  viewer whose offer came through `POST /api/webrtc` (go2rtc's name), `rtsp`,
  `mse` otherwise. openIVR decides whether a camera is held by looking for
  `preload`. A consumer whose request carried a `User-Agent` lists it as
  `user_agent` (cleaned, at most 256 characters; a header that is not text is
  left out): openIVR names each WHEP session `openIVR-whep/<session id>`. A viewer's `medias` is empty here where go2rtc lists the tracks it sends (`video, sendonly, H264`, measured 2026-11-16); openIVR reads `medias` of preloads only.
- oxrtc only, ignored by go2rtc clients: `consumers[]` also carry `frames`,
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
  to the very picture of the source; a picture RFC 2435 cannot describe is not
  sent (R41, R42). go2rtc describes it with payload type 96.
- `queue=live` or `queue=recorder` in that query names the reader's class
  (oxrtc only, D18; go2rtc ignores keys it does not know). Without it an RTSP
  reader is a recorder (10 s or 32 MiB, `rtsp.queue_*`); a reader that shows
  what it reads, such as openIVR's analysis decoder, says `?video&queue=live`
  and is cut to a keyframe after 2 s or 8 MiB (`webrtc.queue_*`) instead of
  being left seconds behind. Any other value, or two different ones, is
  refused with `400`. The reader's `User-Agent` is listed as the consumer's
  `user_agent`.
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
| `http://…`, `https://…` | a webcam serving Motion JPEG (below); `#timeout=N` = stall timeout in seconds, `#interval=MS` = pause between two requests of a camera that serves one picture per request (10 to 60000, default 100), `#insecure` = do not check an `https` camera's certificate; declares `video/jpeg` |

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
Up to 5 redirects are followed. `https` verifies the certificate against the
operating system's trusted roots and the host the URL names. **go2rtc 1.9.15
verifies nothing** (measured: it served a picture from a self-signed local
server), so a camera with a certificate of its own that works there is refused
here (`TLS: invalid peer certificate: UnknownIssuer`) until its source says
`#insecure`, which takes any certificate and logs a warning at each connection.
The track is `video, recvonly, JPEG` in `/api/streams`: no sound, which is
what openIVR's `_has_sound` reads.

Read back over RTSP (`rtsp://127.0.0.1:P/camera_N_src?video`: the H.264
transcode, the recorder's re-encode and the analysis all read it so) a
JPEG is sent as RTP/JPEG (RFC 2435, payload type 26, `m=video 0 RTP/AVP 26`),
the pictures untouched. RFC 2435 describes a picture by a few numbers and
leaves the rest to the receiver, so a picture it cannot describe is not
sent (a gap, never a corrupt picture; one warning per reader): baseline
(not progressive) with 8-bit samples, three components with Y at 2x1 or 2x2
and the chroma at 1x1 (4:2:2 as libjpeg writes it, 4:2:0), the standard
Huffman tables of ITU-T T.81 Annex K, 8-bit quantization tables, a side of
at most 2040 pixels. A picture with the encoder's own Huffman tables
(FFmpeg's `-huffman optimal`, `libjpeg` with `optimize_coding`) is the
case that matters: R41. `frame.jpeg` of such a stream returns the latest
picture as it is, whatever it is.

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
| Stop: `terminate()` on Windows, SIGTERM on Linux, 5 s, then kill | `managed_server.py:330-350` | `e2e.rs` `stop` (listeners and FFmpeg gone within 5 s); Ctrl+Break is R14 | ok |
| The child is put in a kill-on-close job object of the backend right after the spawn, and in a console group of its own: oxrtc's job for each FFmpeg is nested in that one | `process_guard.py:243`, `go2rtc_manager.py:85-102` | `killed.rs` `inside_its_parents_job_a_killed_oxrtc_still_takes_its_ffmpeg_along` (no `no job object` warning, FFmpeg gone) | new |
| A killed oxrtc takes its FFmpeg along (go2rtc's outlive it) | `run_all.py:448` kills them by name | `killed.rs` `a_killed_oxrtc_takes_its_ffmpeg_along_within_a_second` | ok |
| Exit by itself: logged, restarted on the shared back-off, every stream registered again from the new file, holds forgotten | `managed_server.py:360-420`, `go2rtc_manager.py:1169` | openIVR's `tests/integration/test_go2rtc_process.py` against oxrtc, 4 of 4 (2026-11-15) | ok |
| Stale sweep: every process running exactly the binary's file is killed before a start and at a stop; `oxrtc-exec` is another file and never matched | `process_guard.py:50` | `oxrtc-exec` execs FFmpeg, so no stub outlives a stop | ok |

### Configuration written to `config/go2rtc.yaml`

| Behaviour | openIVR | oxrtc | Status |
|---|---|---|---|
| `api: {listen: 127.0.0.1:P, username: openivr, password: <derived>, local_auth: true}` | `go2rtc_manager.py:1840` | `apps/oxrtc/tests/openivr_files.rs` `what_openivr_writes_is_read_whole_and_without_a_warning`; `config.rs` `local_auth_true_is_what_oxrtc_does_and_false_is_not_followed` | new |
| `rtsp: {listen: 127.0.0.1:P}`, `webrtc: {listen: ":P", candidates: [<lan ip>]}` | `:1846-1862` | the same | new |
| `webrtc.ice_servers: [{urls: [stun:, turn:?transport=udp, turn:?transport=tcp], username, credential}]`, one entry, rewritten and the server restarted at each rotation | `:1741-1757`, `main.py:321` | `openivr_files.rs` `the_ice_servers_openivr_writes_are_read_as_one_entry`; `app.rs` `the_ice_servers_reach_the_webrtc_server_as_openivr_writes_them`; `remote.rs`. The `transport=tcp` URL is logged and skipped (D13, **R3**) | ok |
| `ffmpeg: {bin: <Windows path>, h264: <template>, dedup_input: <template>}` | `:1863-1891` | `openivr_files.rs` (backslashes kept, both templates read) | new |
| `log.level` | `:1892` | `logging.rs` `levels` | ok |
| `streams:` in PyYAML flow style, sorted keys: a `_src`, a `_rec` and a public stream per camera, a `_sub` for a camera that declares a substream | `:1789-1806` | `openivr_files.rs` (five cameras, 16 streams; the server starts on the file and serves each of the 16 streams, the webcams' own sources included) | new |
| The same file after go2rtc saved it (block lists, a `preload:` section, credentials stripped): the same sources, the section warned and ignored | `go2rtc_manager.py:563-578` (why flow style) | `openivr_files.rs` `a_file_go2rtc_wrote_back_reads_the_same_streams` | new |
| The file is rewritten whole at every start, so a stream an API call added and go2rtc saved is gone: oxrtc never writes the file | `:1902-1925` | `config.rs`: "read, never written back" | ok |

### Sources of a stream

| Behaviour | openIVR | oxrtc | Status |
|---|---|---|---|
| `rtsp://user:pass@host:554/path#timeout=15`; `#timeout=` added only when the URL has no `#` | `go2rtc_manager.py:217-225` | `crates/rtsp-client/tests/fake_camera.rs`, `refused_credentials_are_auth_and_travel_decoded_outside_the_url` | ok |
| `http(s)://` camera (an MJPEG webcam: three of the owner's six cameras, two plain `http`, one `https`) as the only source of `camera_N_src`, with no `#` option | `camera_url.py:30`, `go2rtc_manager.py:1789`, `:217-225` | `crates/http-client` (`tests/webcam.rs`: stream, picture per request, Basic, Digest, redirects, stalls, the bounds; `tests/https.rs`; `multipart_tests.rs`); `crates/rtsp-server/tests/jpeg.rs` (FFmpeg decodes the RTP/JPEG to the same picture as the file, 4:2:0, 4:2:2, restart intervals); `apps/oxrtc/tests/webcam.rs` (the binary, in openIVR's layout: `camera_N_src?video` read by ffprobe as `mjpeg,640,360`, the recorder's re-encode, `/api/streams` listing `video, recvonly, JPEG`, `frame.jpeg`, a WHEP viewer through the H.264 transcode, the recorder's hold refused for sound then taken on the video, no password in the log or the listing) | ok (R1, D19); pictures with custom Huffman tables **R41** |
| `rtsps://` camera, which the TLS policy `preferred` rewrites a camera to when it answers in TLS | `tls_policy.py:235-300`, `go2rtc_manager.py:895-908` | `PUT` answers 200, the source fails for good (`unsupported: rtsps`, `rtsps_fails_for_good_instead_of_retrying`) | **R8** |
| `rtsp://127.0.0.1:<own port>/camera_N_src?video#timeout=20`, and `…/camera_N_rec?audio=opus`: relays between streams | `:228-249`, `:402-428` | aliases (D14): `crates/core/tests/relay.rs`, `aliases.rs`; `start.rs` `a_loopback_url_on_our_own_port_is_read_in_process` | ok |
| `ffmpeg:camera_N_src#video=h264#width=1920#height=1080#timeout=20` with the `h264` template of the file | `:420-428` | `crates/ffmpeg/tests/transcode.rs` `a_stream_by_name_is_transcoded_to_h264_and_scaled`; `openivr_layout.rs` | ok |
| `ffmpeg:rtsp://127.0.0.1:P/camera_N_src#audio=opus#timeout=20`: the plain sound transcode, both tracks in, Opus out | `:395-399` | `transcode.rs` `pcma_from_an_rtsp_url_is_transcoded_to_opus`; `openivr_layout.rs` | ok |
| `ffmpeg:http://127.0.0.1:<port>/camera_N_src#audio=opus#input=dedup_input`: an endless WAV of G.711 at 16 kHz over HTTP/1.0, through the named input template; a `415` makes the run fail | `audio_dedup.py:228-247, 470-490`, `:1890` | `transcode.rs` `the_deduplicated_sound_served_over_http_is_transcoded_with_openivrs_input_template`, `a_deduplicator_that_answers_415_fails_the_run` | new |
| A stream's sources are tried in order and a failing one falls through to the next (the de-duplicated sound, then the plain one; the picture does not wait for the sound) | `:362-399` | `crates/core/tests/fallback.rs` | ok |
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
| The offer's `User-Agent: openIVR-whep/<id>` is the consumer's `user_agent` in `GET /api/streams`; the consumer is `webrtc/whep` | `:1664`, `whep_sessions.py:19` | `routes.rs` `webrtc_route` and `streams_json_shape`; `whep.rs` | new |
| A `404` whose body says `stream not found` makes openIVR register that camera and retry once; a `500` whose body holds `streams:` is "camera unreachable" (503), any other failure a `502` | `:1701-1714` | `routes.rs` `webrtc_route` (the `404` and the `streams: …` `500`) | ok |
| A viewer's consumer goes when the browser closes its peer connection: go2rtc has no call to remove one, and openIVR relies on that (the `DELETE` of its own resource only forgets the session) | `whep_sessions.py:3-20`, `streaming.ts:536-560` | `whep.rs` (`client.stop(Control::Close)`: session and consumer gone); the WHEP resource itself is a separate item | ok |
| The Basic credential on every call, the loopback ones included; none is `401` | `:744`, `:1840` | `routes.rs` `every_route_requires_authentication` | ok |
| The browser with no usable candidate in its offer (IPv6 filtered, mDNS names): the server never starts checks, and learns the viewer's address from its own | `streaming.ts:427` | ICE-lite, R27 `a_binding_request_reaches_its_session_and_teaches_it_the_route`; `candidates.rs:221` (an mDNS name is read and skipped); every test of `whep.rs` | ok |

### RTSP readers (`rtsp.listen`, TCP interleaved)

| Reader | openIVR | oxrtc | Status |
|---|---|---|---|
| The recorder's segmenter: FFmpeg, `-rtsp_transport tcp -timeout 20 s -fflags +genpts+discardcorrupt -i rtsp://127.0.0.1:P/camera_N_rec?video=copy&audio=opus` (or `?video=copy` for a camera held by its picture), `-map 0:v:0 -map 0:a:0?`; it must lose nothing when paused for seconds | `segmenter.py:405-436`, `:1959-1987` | `crates/rtsp-server/tests/ffmpeg.rs`; `paused_reader.rs` `a_reader_paused_5_s_loses_nothing`; `protocol.rs` `queries_select_the_described_media` | ok |
| The analysis decoder (OpenCV's FFmpeg and the Go sidecar's FFmpeg): `-rtsp_transport tcp -i rtsp://127.0.0.1:P/camera_N_sub?video` (or `camera_N?video`, or `camera_N` for the sidecar). It shows what it reads, so it wants the live budget (2 s), not the recorder's (10 s) | `go2rtc_manager.py:1989-2042`, `frame_capture.py:245-275`, `sidecar/internal/source/stream.go:330-390` | `queue=live` (D18): `paused_reader.rs` `a_reader_that_asks_for_the_live_queue_is_cut_to_a_keyframe`. Until openIVR adds `&queue=live` to `analysis_url()` it is a recorder | **R36** |
| The sound de-duplicator: its own RTSP session (`audio_dedup.py`): the picture described alone on a connection kept open, the plain URL on a second, the sound alone set up on the channel pair twice its section's index, `PLAY` on the URL without the trailing slash, `OPTIONS` in-band every 20 s, `User-Agent: openIVR-audio` | `audio_dedup.py:140-460` | `crates/rtsp-server/tests/openivr_readers.rs` `the_sound_deduplicators_session_carries_the_sound_alone`, every step replayed | new |
| No query: every track the stream can give, the picture first (the capture sidecar's `camera_N`); a query naming one media excludes the other. **Differs from go2rtc on openIVR's layout** (measured 2026-11-16, 1.9.15, the three streams openIVR writes for one camera with sound): `rtsp://…/camera_N` and `…/camera_N?video&audio` answer the picture alone, the sound being started only by a consumer that names its codec (`audio=opus`, a browser's offer); oxrtc answers both. The sidecar's FFmpeg drops the sound (`-an`), so the cost is the sound transcode it starts when nothing holds `camera_N_rec` with sound, and on a camera with no sound a DESCRIBE that waits for the sound's walk (measured cold: 7.7 s in oxrtc, 12.0 s in go2rtc, so not a regression). The cure is openIVR's: the sidecar's URL says `?video` (R36) | `go2rtc_manager.py:1953`, `:2042`; `capture_sidecar.py`, `main.py:374` (`relay_url`) | `protocol.rs` `queries_select_the_described_media` | ok, **R36** |
| A camera restarted or a sound transcode restarted under a steady picture does not move a long-lived reader's clock; sender reports dated by arrival | `segmenter.py:370-395` | `crates/rtsp-server/tests/timeline.rs` (D12) | ok |
| A reader asking for a source that is still cold waits for its tracks; the first reader decides nothing about the order | `go2rtc_manager.py:228-249` | `crates/core/tests/fallback.rs` `the_first_track_list_arrives_within_the_deadline_of_the_subscribe_call`; tracks always video first | ok |

### openIVR's side of the switch (changes in openIVR, not in oxrtc)

| Behaviour | openIVR | What must change | Status |
|---|---|---|---|
| The running version is compared with `deps/GO2RTC_VERSION` and a warning says to run `install.py` to replace `bin/go2rtc`; the system page shows the version as `go2rtc` | `go2rtc_manager.py:767-787`, `system.py:159-169` | oxrtc reports `0.1.0`: the check must name the server it expects (a spurious warning at every start otherwise) | **R37** |
| `install.py` downloads go2rtc by a pinned version and checks its SHA-256 (`deps/GO2RTC_SHA256SUMS`); `Dockerfile` does the same | `install.py:1525-1700`, `Dockerfile:97-123` | an oxrtc archive per platform (`oxrtc` and, on Linux, `oxrtc-exec` beside it) with its own pin; a release of a private repository cannot be downloaded without a token | **R38** |
| `run_all.py` kills stale processes by image name on Windows (`ffmpeg.exe`, `go2rtc.exe`) and by project ownership elsewhere | `run_all.py:405-460, 985-995` | add `oxrtc.exe` (and `oxrtc-exec`) to both | **R37** |
| `/health` reports `checks.go2rtc`, and the updater warns when it is `false` | `main.py:1363`, `updater/runner.py:69` | a key named after go2rtc: a rename is a change of both sides, otherwise nothing | ok |
| The first run of a new executable on Windows: Windows Defender Firewall asks, per program path, whether `oxrtc.exe` may listen (the rule go2rtc has is `go2rtc.exe`'s, at its path; the owner's machine holds one such rule per copy that ever listened). An unanswered prompt on a machine that starts the NVR at logon leaves the WebRTC port (`webrtc.listen`, TCP and UDP) closed to every other device on the LAN, while a browser on the same machine still plays: the phone and the other PCs lose the live view and nothing in either log says why | `go2rtc_manager.py:1846` (`webrtc.listen`), `install.py` (no firewall step) | nothing in oxrtc can answer it: a rule is the installer's (`netsh advfirewall firewall add rule name="oxrtc" dir=in action=allow program=<bin\oxrtc.exe> enable=yes profile=private,public`, administrator) or the operator's click | **R40** |
