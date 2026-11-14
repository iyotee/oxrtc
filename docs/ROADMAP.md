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

- [x] Fuzzing of the RTSP server parser, SDP handling and the packetizers - `fuzz/` (seven cargo-fuzz targets, [fuzz/README.md](../fuzz/README.md)) over harnesses in `crates/rtsp-server/src/fuzzing.rs`, the same harnesses under proptest in `cargo test`, seeds replayed there too; found the sdp-types 0.1 panic on a malformed `ANNOUNCE` body (fixed by sdp-types 0.2), an overflow of retina's 16-bit loss counters on large RTP sequence gaps (bounded in `depacketize.rs`), and depacketized frames up to five times over the frame cap (now refused); a `--release` soak only sees overflows with `CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true`. `fuzz/Cargo.lock` is committed and held to the workspace's (`fuzz_lock_follows_the_workspace_lock`). Not yet done: the hour per target of SEC-010 (first run of `fuzz.yml`; libFuzzer does not link on the development machine)
- [ ] Per-IP limits on every listener, request timeouts (per-IP limits on the API, RTSP and WHEP: done, R9). Not yet: a deadline on a whole RTSP request (the silence timeout restarts at every byte, so a trickle holds its connection until the 64 KiB cap), and the RTSP *total* limit still warns once per refused connection (`crates/rtsp-server/src/lib.rs`, "connection limit reached"), where the per-address refusals warn at most once a second (`PerIpLimit::refused`)
- [x] `cargo deny` and `cargo audit` in CI, SBOM per release - `deny.toml`; `.github/workflows/ci.yml` (fmt, clippy and tests on Linux and Windows, then `cargo deny check` and `cargo audit`); `release.yml` writes a CycloneDX SBOM per binary. Both checks pass locally; the workflows passed actionlint but have not run on GitHub yet (Actions minutes blocked on the account)
- [ ] Release builds for linux-amd64, linux-arm64 (Raspberry Pi 5), windows-amd64 - `.github/workflows/release.yml` (tag `v*`: three archives, their SBOMs, `SHA256SUMS`); windows-amd64 builds locally, the two Linux builds (arm64 cross-compiled, C compiler included for aws-lc-sys, R15) wait for a first run

## Later (R = roadmap items)

- R1 `http(s)://` MJPEG and JPEG-poll sources (webcams)
- R2 Egress policy for camera URLs (allowed ranges, DNS pinning)
- R3 TURN client for remote viewers (`webrtc.ice_servers`)
- R4 MSE/fMP4 over WebSocket, HLS
- R5 ONVIF discovery
- R6 Two-way audio (backchannel)
- R7 Native API v1 with per-stream permissions
- R8 RTSPS camera ingest: TLS in retina upstream, or a TLS stream handed to it
- [x] R9 Per-IP connection limits on RTSP and WHEP (SEC-006), a config knob - done: the API, the RTSP server and WHEP sessions count per peer address (`oxrtc_core::PerIpLimit`, D9), with `api.max_connections_per_ip`, `rtsp.max_connections_per_ip` and `webrtc.max_sessions_per_ip`
- [x] R10 Incremental RTSP request parsing - done: each byte is examined once and a request parsed at most three times (`crates/rtsp-server/src/framing.rs`); `work_stays_linear_on_a_slow_sender` pins it
- R11 A byte cap on retina's in-progress access unit (upstream change, or our own demux loop)
- R12 `ffmpeg:` features refused today by D5/D6: `#hardware=` encoders, `#drawtext=`, outputs go2rtc pipes to stdout (`#video=mjpeg/raw`, `#audio=pcml/mp3/auto`), file/device/rtmp inputs
- R13 Off Windows, tie FFmpeg children to oxrtc (`PR_SET_PDEATHSIG` or a process group): a SIGKILL of oxrtc leaves them until their sockets break
- R14 Windows graceful stop (Ctrl+Break) tested from a child process; `terminate()`, which openIVR uses, is tested
- R15 C crypto in the graph: str0m-rust-crypto turns on dimpl's `rcgen` feature, which forces `aws-lc-rs`/`aws-lc-sys` (C, for the DTLS certificate). D3's "pure Rust" holds for DTLS and SRTP, not for that. Ends when dimpl can generate its certificate with rust-crypto (an upstream change)
- R16 Third-party notices in the release archives: Apache-2.0, BSD-3-Clause and ISC (aws-lc among others) ask for their notices with a binary; the SBOM names the licences but does not carry the texts (`cargo about` or equivalent in `release.yml`)
- R17 Duplicate crate versions `cargo deny` warns about (windows-sys, socket2, rand/getrandom, syn, thiserror, the sha2/digest family, nom): each pulled by a dependency's own pin; to revisit on each upgrade
- R18 Fuzzing on a schedule: `fuzz.yml` runs only when started by hand. Seven targets at an hour each, weekly, is about 28 runner-hours a month, most of a private account's Actions minutes; a schedule (or a shorter one per target) is the owner's call
- R19 Upstream the h264-reader level relaxation (D10, `third_party/README.md`): an issue or a pull request on h264-reader (or retina) so that an SPS over its declared level's MaxFS is read; once released, delete `third_party/h264-reader` and the `[patch.crates-io]` entry

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
| SEC-006 | Connection and request limits on every listener | load tests; per-address limits: `connections_beyond_the_per_address_limit_are_closed`, `per_address_connection_limit`, `max_sessions_per_ip_is_enforced`, `the_per_address_keys_limit_the_listeners` |
| SEC-007 | No command execution path | grep in CI + `exec:` refused |
| SEC-010 | Parsers fuzzed, no crash in 1 h per target | `cargo fuzz` in CI (on-demand job, `.github/workflows/fuzz.yml`); the same harnesses under proptest in `cargo test` (`fuzzing_tests.rs`, `offer_props.rs`) |
| PERF-001 | Pass-through RSS per stream ≤ go2rtc's on the reference scenario | bench B03 |
| PERF-002 | Pass-through CPU per stream ≤ go2rtc's | bench B03 |
| PERF-003 | Time to first frame for a warm stream < 300 ms | bench L02 |
| PERF-004 | Glass-to-glass latency not worse than go2rtc's by more than its confidence interval | bench L01 |
