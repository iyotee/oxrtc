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
  ice_servers:                        # TURN for remote viewers (see the roadmap)
    - urls: ["turn:relay.example:3478"]
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
| `ffmpeg.max_processes` | 16 | FFmpeg processes at once, stills included; go2rtc reads it as a template nothing names |

The limits are whole numbers of at least 1; a per-address one is effective up
to its listener's total (the API and RTSP totals, 256 each, are fixed). Loopback is counted like any address, a global or
unique-local IPv6 peer by its `/64` (so IPv6 viewers on one LAN subnet share
one bound), a link-local IPv6 peer by its own address.

## HTTP API

All on `api.listen`. JSON bodies are UTF-8; request bodies are capped (64 KiB,
SDP included). At most 256 connections are open at once (more wait in the
accept queue) and 128 from one address (more are closed on accept).

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
  (`413` and the connection closes beyond), however slowly it arrives.
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

## Sources

| Spec | Meaning |
|---|---|
| `rtsp://…`, `rtsps://…` | a camera, over TCP; `#timeout=N` = stall timeout in seconds |
| `rtsp://127.0.0.1:<own port>/<name>?<query>` | another stream of this server: becomes `stream:<name>?<query>`, no network hop |
| `ffmpeg:<stream name>#video=h264#width=W#height=H` | transcode of a stream of this server; declares `video/h264` |
| `ffmpeg:<url>#audio=opus#input=<template>#timeout=N` | transcode of a URL (rtsp, http) with a named input template; declares `audio/opus` |
| `http(s)://…` (MJPEG) | a webcam; roadmap R1 |

`exec:`, `echo:`, `expr:` and every scheme not listed are refused with `400`.

## WebRTC

- At most 64 sessions at once (`webrtc.max_sessions`) and 32 for one caller
  address (`webrtc.max_sessions_per_ip`, the HTTP peer of `POST /api/webrtc`);
  beyond either the offer is answered `503`.
- WHEP over `POST /api/webrtc`. The answer is complete (no trickle): host
  candidates for every local address plus `webrtc.candidates`, on the single
  UDP port of `webrtc.listen` (and TCP on the same port when enabled).
- The offer's codecs choose the source: a browser offering H.265 gets the
  camera's own stream; one offering only H.264 gets the transcode. Audio: Opus
  (PCMA/PCMU when offered and the source has them).
- IPv6 candidates: openIVR strips them from both offer and answer; emit IPv4
  first.
