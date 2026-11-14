# Roadmap

Milestones gate on **acceptance criteria**, not dates. Estimates assume one
maintainer with agent assistance.

## M1 - Pass-through gateway (target: December 2026)

A camera visible in a browser through oxrtc, under openIVR, unchanged except
for the API credential.

- [x] Core: streams, fan-out, GOP cache, supervision (`crates/core`)
- [x] RTSP ingest with retina, TCP, digest auth, reconnects (RTSPS: retina has no TLS - see R8)
- [x] Local RTSP server: DESCRIBE/SETUP/PLAY over TCP, `?video`/`?audio` filters
- [x] WHEP egress with str0m: H.264, H.265, Opus, PCMA/PCMU
- [x] go2rtc-compatible API: streams, preload, webrtc
- [x] Binary: `-config`, go2rtc log format, graceful stop (`apps/oxrtc`)

## M2 - Feature parity for openIVR (target: January 2027)

- [x] `ffmpeg:` transcodes (video to H.264, audio to Opus), templates, `#input=` (`crates/ffmpeg`; refusals listed in R12)
- [x] RTSP RECORD for the transcoders' publications
- [x] `GET /api/frame.jpeg`
- [ ] openIVR's full test suite green against oxrtc in place of go2rtc
- [ ] Bench phase B complete ([BENCHMARKS.md](BENCHMARKS.md))

## M3 - Hardened (target: February 2027)

- [ ] Fuzzing of the RTSP server parser, SDP handling and the packetizers
- [ ] Per-IP limits on every listener, request timeouts
- [ ] `cargo deny` and `cargo audit` in CI, SBOM per release
- [ ] Release builds for linux-amd64, linux-arm64 (Raspberry Pi 5), windows-amd64

## Later (R = roadmap items)

- R1 `http(s)://` MJPEG and JPEG-poll sources (webcams)
- R2 Egress policy for camera URLs (allowed ranges, DNS pinning)
- R3 TURN client for remote viewers (`webrtc.ice_servers`)
- R4 MSE/fMP4 over WebSocket, HLS
- R5 ONVIF discovery
- R6 Two-way audio (backchannel)
- R7 Native API v1 with per-stream permissions
- R8 RTSPS camera ingest: TLS in retina upstream, or a TLS stream handed to it
- R9 Per-IP connection limits on RTSP and WHEP (SEC-006), a config knob
- R10 Incremental RTSP request parsing (today each read reparses the buffer: quadratic on a slow sender)
- R11 A byte cap on retina's in-progress access unit (upstream change, or our own demux loop)
- R12 `ffmpeg:` features refused today by D5/D6: `#hardware=` encoders, `#drawtext=`, outputs go2rtc pipes to stdout (`#video=mjpeg/raw`, `#audio=pcml/mp3/auto`), file/device/rtmp inputs
- R13 Off Windows, tie FFmpeg children to oxrtc (`PR_SET_PDEATHSIG` or a process group): a SIGKILL of oxrtc leaves them until their sockets break
- R14 Windows graceful stop (Ctrl+Break) tested from a child process; `terminate()`, which openIVR uses, is tested

## Acceptance criteria

| ID | Criterion | How it is checked |
|---|---|---|
| CORE-001 | No panic on any input from a source or a peer | `clippy` denies `unwrap`/`expect`/indexing outside tests; fuzz targets |
| CORE-002 | Every buffer bounded | tests on `Limits` (ring, GOP, backlog, frame size) |
| CORE-003 | A slow consumer never stalls the producer | `a_slow_consumer_loses_frames_and_resumes_on_a_keyframe` |
| CORE-004 | Sources stop when unused, restart when failing | `sources_run_on_demand_…`, `a_failing_source_…`, `a_stalled_source_…` |
| CORE-005 | No task leak over 24 h | soak test: task count and RSS flat |
| SEC-001 | Every API call authenticated, loopback included | API tests |
| SEC-004 | A source reaches only what its scheme implies | test suite of R2 |
| SEC-005 | No credential in logs, errors, API output | property test + log capture |
| SEC-006 | Connection and request limits on every listener | load tests |
| SEC-007 | No command execution path | grep in CI + `exec:` refused |
| SEC-010 | Parsers fuzzed, no crash in 1 h per target | `cargo fuzz` in CI (nightly job) |
| PERF-001 | Pass-through RSS per stream ≤ go2rtc's on the reference scenario | bench B03 |
| PERF-002 | Pass-through CPU per stream ≤ go2rtc's | bench B03 |
| PERF-003 | Time to first frame for a warm stream < 300 ms | bench L02 |
| PERF-004 | Glass-to-glass latency not worse than go2rtc's by more than its confidence interval | bench L01 |
