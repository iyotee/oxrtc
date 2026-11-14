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
- **Slot**: demand counting and the supervisor. First demand starts the driver;
  last release stops it after `idle_grace`. A run that fails, ends, produces
  nothing for `connect_timeout` or nothing more for `stall_timeout` is
  restarted after exponential backoff with ±25 % jitter, capped at
  `backoff_max` (5 s: a camera that comes back is retried within 5 s; 30 s
  cost 18 s of recording after a 60 s outage in the bench's R01); backoff
  resets after `healthy_after` of good media.
- **Channel**: one producer, many consumers. A `tokio::sync::broadcast` ring of
  `ring_capacity` items carries track lists and frames in order. A lagging
  consumer gets `Discontinuity` and its video resumes on the next keyframe. The
  channel also keeps the **current GOP** (up to `gop_cache_bytes`): a new
  subscriber receives it first, so it decodes at once.
- **Subscription**: merges one or two slots (e.g. video from the camera, Opus
  from a transcoder) into one event sequence. The first event is always
  `Tracks`; frames that arrive before every route is resolved are held
  (bounded) and released in order.

Frames are access units; H.264/H.265 payloads are length-prefixed NAL units,
validated on publish. Timestamps are in the track's clock rate.

## Data path and copies

A camera frame is allocated once (by the depacketizer) as `Bytes` and shared by
`Arc` with every consumer. Egress packetizes per consumer (RTP for RTSP,
str0m for WebRTC). There is no decode in the pass-through path.

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
