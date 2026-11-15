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
- [x] openIVR's full test suite green against oxrtc in place of go2rtc (2026-11-15, openIVR v2026.11.15.1): its one file that runs a real media server, `tests/integration/test_go2rtc_process.py`, passes 4 of 4 with `OPENIVR_MEDIA_SERVER_BIN=target/release/oxrtc.exe` (a kill brings it back with its streams, a stop keeps it stopped, a loopback caller without the credential is refused, the live view's WHEP offer is answered before and after a kill); every other openIVR suite runs on fakes of the media server
- [ ] Bench phase B complete ([BENCHMARKS.md](BENCHMARKS.md))

Found by the review of 2026-11-15 (code read, not yet reproduced): the e2e
test and the bench serve a simpler layout than the one openIVR writes, and
on openIVR's layout these would be outages at switch-over.

- [x] openIVR's exact stream layout as an e2e fixture (`apps/oxrtc/tests/openivr_layout.rs`, a camera with G.711 sound at 16 kHz and one without a microphone; the de-duplicated sound source is left out). And in the bench, scenario B07 (`layout: openivr`, `bench/servers/go2rtc.py` `openivr_streams`, held to openIVR's source by `bench/tests/test_layout.py`). The layout: the `_src` and `_rec` relays, the public stream's three sources (sound third), an audio transceiver in every offer, 16 kHz audio, openIVR's `h264` template (today `apps/oxrtc/tests/e2e.rs` has two sources and no audio; `bench/servers/go2rtc.py` one copy, one substream, one transcode)
- [x] A camera whose sound cannot be produced still plays: `Stream::first_offer` (`crates/core/src/stream.rs`) probes an `Unknown` slot for the whole `probe_timeout` (10 s), which equals the WHEP answer deadline (`crates/webrtc/src/lib.rs`) wrapped around the whole subscribe (`crates/webrtc/src/server.rs`), so the answer times out first. End a probe on the slot's first failed attempt, keep the probe budget under the answer budget, answer once video is ready and let audio join
- [x] Failover between sources that declare the same codec (at selection; a source that fails after it was picked is not replaced yet): `Slot::offers` (`crates/core/src/slot.rs`) answers `Yes` from a declared codec without the source ever running, the first `Yes` wins and a failing run retries forever; openIVR relies on go2rtc falling through to the next source (its de-duplicated sound answers 415 for non-G.711 audio)
- [x] `ffmpeg.max_processes` (default now 64; B07 showed 16 starving cameras 9 and up): the cap of 16 (`crates/ffmpeg/src/lib.rs`) was not configurable, and every `ffmpeg:` key but `bin` is read as a template (`crates/api/src/config/yaml.rs`); openIVR holds one sound transcode per camera, so 16 cameras with sound leave no room for a video transcode, refused quietly in a retry loop

## M3 - Hardened (target: February 2027)

- [x] Fuzzing of the RTSP server parser, SDP handling and the packetizers - `fuzz/` (seven cargo-fuzz targets, [fuzz/README.md](../fuzz/README.md)) over harnesses in `crates/rtsp-server/src/fuzzing.rs`, the same harnesses under proptest in `cargo test`, seeds replayed there too; found the sdp-types 0.1 panic on a malformed `ANNOUNCE` body (fixed by sdp-types 0.2), an overflow of retina's 16-bit loss counters on large RTP sequence gaps (bounded in `depacketize.rs`), and depacketized frames up to five times over the frame cap (now refused); a `--release` soak only sees overflows with `CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true`. `fuzz/Cargo.lock` is committed and held to the workspace's (`fuzz_lock_follows_the_workspace_lock`). Not yet done: the hour per target of SEC-010 (first run of `fuzz.yml`; libFuzzer does not link on the development machine)
- [ ] Per-IP limits on every listener, request timeouts (per-IP limits on the API, RTSP and WHEP: done, R9). Not yet: a deadline on a whole RTSP request (the silence timeout restarts at every byte, so a trickle holds its connection until the 64 KiB cap), and the RTSP *total* limit still warns once per refused connection (`crates/rtsp-server/src/lib.rs`, "connection limit reached"), where the per-address refusals warn at most once a second (`PerIpLimit::refused`)
- [x] `cargo deny` and `cargo audit` in CI, SBOM per release - `deny.toml`; `.github/workflows/ci.yml` (fmt, clippy and tests on Linux and Windows, then `cargo deny check` and `cargo audit`); `release.yml` writes a CycloneDX SBOM per binary. Both checks pass locally; the workflows passed actionlint but have not run on GitHub yet (Actions minutes blocked on the account)
- [ ] Release builds for linux-amd64, linux-arm64 (Raspberry Pi 5), windows-amd64 - `.github/workflows/release.yml` (tag `v*`: three archives, their SBOMs, `SHA256SUMS`); windows-amd64 builds locally, the two Linux builds (arm64 cross-compiled, C compiler included for aws-lc-sys, R15) wait for a first run

## Later (R = roadmap items)

- R1 `http(s)://` MJPEG and JPEG-poll sources (webcams)
- R2 Egress policy for camera URLs (allowed ranges, DNS pinning)
- R3 TURN client for remote viewers (`webrtc.ice_servers`, ignored today by `apps/oxrtc/src/app.rs`), and ICE-TCP. A parity item rather than a later one: openIVR's Remote Access writes `ice_servers` and restarts the media server at each credential rotation, so a runtime update of the ICE servers would also stop a rotation from cutting every recording
- R4 MSE/fMP4 over WebSocket, HLS
- R5 ONVIF discovery
- R6 Two-way audio (backchannel)
- R7 Native API v1 with per-stream permissions
- R8 RTSPS camera ingest: TLS in retina upstream, or a TLS stream handed to it
- [x] R9 Per-IP connection limits on RTSP and WHEP (SEC-006), a config knob - done: the API, the RTSP server and WHEP sessions count per peer address (`oxrtc_core::PerIpLimit`, D9), with `api.max_connections_per_ip`, `rtsp.max_connections_per_ip` and `webrtc.max_sessions_per_ip`
- [x] R10 Incremental RTSP request parsing - done: each byte is examined once and a request parsed at most three times (`crates/rtsp-server/src/framing.rs`); `work_stays_linear_on_a_slow_sender` pins it
- R11 A byte cap on retina's in-progress access unit (upstream change, or our own demux loop): no upstream change is needed, since retina exposes the raw packet stream; moving `TrackDepacketizer` (`crates/rtsp-server/src/depacketize.rs`, already capped) into a shared crate and feeding it from that stream gives one depacketize path and the camera's sender reports (R20)
- R12 `ffmpeg:` features refused today by D5/D6: `#hardware=` encoders, `#drawtext=`, outputs go2rtc pipes to stdout (`#video=mjpeg/raw`, `#audio=pcml/mp3/auto`), file/device/rtmp inputs
- R13 Off Windows, tie FFmpeg children to oxrtc (`PR_SET_PDEATHSIG` or a process group): a SIGKILL of oxrtc leaves them until their sockets break
- R14 Windows graceful stop (Ctrl+Break) tested from a child process; `terminate()`, which openIVR uses, is tested
- R15 C crypto in the graph: str0m-rust-crypto turns on dimpl's `rcgen` feature, which forces `aws-lc-rs`/`aws-lc-sys` (C, for the DTLS certificate). D3's "pure Rust" holds for DTLS and SRTP, not for that. Ends when dimpl can generate its certificate with rust-crypto (an upstream change)
- R16 Third-party notices in the release archives: Apache-2.0, BSD-3-Clause and ISC (aws-lc among others) ask for their notices with a binary; the SBOM names the licences but does not carry the texts (`cargo about` or equivalent in `release.yml`)
- R17 Duplicate crate versions `cargo deny` warns about (windows-sys, socket2, rand/getrandom, syn, thiserror, the sha2/digest family, nom): each pulled by a dependency's own pin; to revisit on each upgrade
- R18 Fuzzing on a schedule: `fuzz.yml` runs only when started by hand. Seven targets at an hour each, weekly, is about 28 runner-hours a month, most of a private account's Actions minutes; a schedule (or a shorter one per target) is the owner's call
- R19 Upstream the h264-reader level relaxation (D10, `third_party/README.md`): an issue or a pull request on h264-reader (or retina) so that an SPS over its declared level's MaxFS is read; once released, delete `third_party/h264-reader` and the `[patch.crates-io]` entry
- [x] R20 (done 2026-11-15, D12) One monotonic, wallclock-anchored timeline per track in the core, applied at publish, shared by every egress. Was: the RTSP egress passed a source's timestamp restart through (`packetize.rs` added a random offset to the raw `pts`), the only continuity logic lived in the WebRTC egress (`payload.rs`, which also missed restarts under a second), and the first sender report left 5 s after DESCRIBE - the go2rtc defect openIVR's segmenter detects (sound hours ahead of its picture). Done: `crates/core/src/timeline.rs`, applied in `Channel::try_publish`, one epoch per hub, kept across reconnections while a consumer is attached, `stream:` relays keep their upstream's time; the WebRTC `Timeline` deleted; the RTSP egress sends a sender report with each track's first packet, dated by `Frame::received`, and derives the later ones from it. Pinned by `crates/rtsp-server/tests/timeline.rs` (a reader across a camera reopened, and across a sound transcode restarted under a steady picture: both failed before, `RTP clock jumped -1.560 s`), `crates/webrtc/tests/timeline.rs`, `crates/core/tests/timeline.rs` and the unit tests and proptest of `timeline_tests.rs`. Not done: anchoring a run on the camera's own sender reports instead of its first frame's arrival (R11), so a run whose first frame came late (keyframes of a bursty camera) is placed that late for the whole run; a transcode started on the cached GOP is anchored at its first output, up to a GOP behind its content; str0m's WebRTC reports still follow each frame's arrival; not yet checked under openIVR's segmenter on the real cameras (phase D)
- R21 `stream:` relays as aliases onto the upstream channel, resolved at selection, instead of a second supervised driver (`crates/core/src/local.rs`): each layer has its own backoff, `#timeout=` is ignored on a loopback URL (`crates/rtsp-client/src/target.rs`), every frame is re-published into a second ring and GOP cache. Needs explicit cycle detection
- R22 (done 2026-11-15: 30 s doubling to 10 min, `Limits::auth_backoff_*`) A longer backoff ceiling for refused credentials: a 401 is retryable (`crates/rtsp-client/src/codec.rs`), so a changed password is a failed login every 5 s, which can lock out cameras that count failures
- R23 Queues bounded by bytes and time per consumer class, not by item count: 256 items is about 4 s for a recorder reading video and Opus (`core` ring, `crates/rtsp-server/src/outbox.rs`), and an overflow drops video until the next keyframe, up to 6 s on the owner's cameras
- R24 A keyframe request from a viewer replays the cached GOP to that session, rate-limited (`crates/webrtc/src/session.rs` logs "no upstream to ask")
- R25 (done 2026-11-15: `apps/oxrtc/src/log_writer.rs`, a queue of 4096 lines; retry warnings at the first failure, a changed error and attempts 2, 4, 8...) A bounded, lossy, non-blocking log writer (`apps/oxrtc/src/logging.rs` writes stdout synchronously from runtime workers) and retry warnings throttled to the first failure, state changes and a count per minute (`crates/core/src/slot.rs`)
- R26 (done 2026-11-15 in `/api/streams`: `sources` and the consumers' counters, docs/COMPATIBILITY.md; a metrics endpoint is not done) Source state, last error, attempts, last frame age, rejected frames and drops in `StreamInfo` (`crates/api/src/info.rs` serializes none of what `Stream::snapshot` holds) and a metrics endpoint; the bench needs these to normalize CPU by delivered work
- R27 Parse a WebRTC datagram from an unknown address once and route STUN by ICE user name: today it is offered to every session in turn, each re-parsing it under its own lock (`crates/webrtc/src/server.rs`)
- R28 RTSP read authentication: refusing to start on a security key that is not honoured is done (2026-11-15, `rtsp.username`/`password`); still to do: implement it with a separate credential for FFmpeg children
- R29 Confine FFmpeg off Windows (with R13): no-new-privileges, Landlock, rlimits and a nice value through a small exec wrapper; D5 says "sandboxed" where `crates/ffmpeg/src/process.rs` isolates (argument vector, cleared environment, kill on drop)
- R30 The main `Limits` as oxrtc-only config keys (`apps/oxrtc/src/app.rs` uses the defaults), and a GOP over `gop_cache_bytes` logged once per source (`crates/core/src/channel.rs` closes the cache silently; 8 MiB is a 6 s GOP at about 10.6 Mbit/s)
- R31 Compatibility details: `api.local_auth` is followed since 2026-11-15 (no warning for `true`); under openIVR every offer comes from 127.0.0.1, so the per-address limit is the effective session limit (D9); a new preload cancels the old one before it holds (`crates/api/src/preload.rs`)
- R32 (done 2026-11-15, D11: 6.1 s to 1.5 s on B07; to confirm on the Pi in phase D) First frame through a transcode (4.4 to 9.7 s in one 30 s B03 smoke run, groups one GOP apart): test whether the `rtsp` input template's `-fflags nobuffer` (`crates/ffmpeg/src/templates.rs`, copied from go2rtc) discards the cached GOP; report `answerMs`, which the probe records. Hypothesis until measured
- R33 Native audio (one de-duplication at ingest, Opus or G.711 produced in-process) instead of openIVR's chain of a Python RTSP session, an HTTP WAV and one FFmpeg per camera. An Opus encoder likely bends D5; measure first
- R34 Bench fairness: CPU normalized by frames delivered, the server split from its FFmpeg children, an audio/video skew measure, a camera server that is not go2rtc, a Pi-shaped scenario (4 to 8 cameras, recorders, a CPU competitor, a cgroup limit), H.265 pass-through
- R35 Phase B's one loss on a hot path, L03 cold start (2.97 s against go2rtc's 2.49 s), is the stale-session wait of `crates/rtsp-client/src/driver.rs` (`MAX_STALE_WAIT`, 1 s): split by when the next offer lands, offers more than 2 s after the source stopped read 2067 ms on both servers, earlier ones pay about 700 ms more on the WHEP answer and 1.2 s on the first frame. The bench's camera is a go2rtc, which never answers TEARDOWN. retina's `SessionGroup` documents why the wait exists (live555 cameras that keep sending on a dead session, and the owner has live555 cameras), so measure the owner's cameras' TEARDOWN answer in phase D before shortening it or skipping it when the previous session was TCP and its connection is closed

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
