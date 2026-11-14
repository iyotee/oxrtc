# oxrtc

A real-time media gateway for IP cameras, written in Rust: RTSP in, WebRTC
(WHEP) and RTSP out, with on-demand sources, bounded memory, and an
administration API that is never open by default.

> Working name. Status: **pre-alpha**. The media core is done and tested; the
> protocol crates are being built. Nothing here is production-ready yet.

## Why another one

go2rtc is excellent and is the reference this project measures itself against.
oxrtc keeps its best ideas - several sources per stream, the codec chosen by
what the viewer accepts, a single camera connection shared by every reader - and
changes three things:

| | go2rtc | oxrtc |
|---|---|---|
| Administration API | trusts loopback and Unix-socket callers without authentication; `exec:` sources run commands | authentication on every call, whatever the source address; no scheme can run a command |
| A new viewer | waits for the camera's next keyframe (up to 6 s on cameras with a long GOP) | starts at once on the cached group of pictures |
| Knowing a transcode's codec | starts the transcoder to find out | the transcode declares its output; nothing starts for nothing |

Every claim of performance is to be **measured**, against go2rtc and MediaMTX,
on the same machine and the same streams - see [docs/BENCHMARKS.md](docs/BENCHMARKS.md).
Until those numbers exist, this README makes none.

## Layout

| Path | What |
|---|---|
| `crates/core` | Media core: streams, fan-out, GOP cache, source supervision. No network I/O. |
| `crates/rtsp-client` | Camera ingest (RTSP/RTSPS), on top of [retina](https://github.com/scottlamb/retina). |
| `crates/rtsp-server` | Local RTSP relay: serves streams, accepts the transcoders' publications. |
| `crates/webrtc` | WHEP egress, on top of [str0m](https://github.com/algesten/str0m) (Sans-I/O, pure-Rust DTLS). |
| `crates/ffmpeg` | Transcoding and stills through a sandboxed FFmpeg subprocess. |
| `crates/api` | HTTP API: the go2rtc-compatible surface and the native one. |
| `apps/oxrtc` | The binary. |
| `bench/` | The comparison bench: go2rtc, MediaMTX, oxrtc. |

## Documents

- [Architecture](docs/ARCHITECTURE.md) and [decisions](docs/DECISIONS.md)
- [Compatibility with go2rtc](docs/COMPATIBILITY.md) - the exact surface a NVR relies on
- [Threat model](docs/THREAT_MODEL.md)
- [Roadmap](docs/ROADMAP.md) and its acceptance criteria
- [Benchmark plan](docs/BENCHMARKS.md)

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
