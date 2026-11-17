# Benchmark plan

Three servers, one bench, one decision: **go2rtc** (in production in openIVR
today), **oxrtc**, **MediaMTX** - and, as a fourth reference, the GPL Rust
reimplementation of go2rtc published in October 2026. The output is a decision
on openIVR's media architecture, made from numbers that anyone can reproduce.

## Phases

| Phase | When | What |
|---|---|---|
| A - Baseline | now | go2rtc 1.9.15 alone, every scenario; freezes the reference |
| B - oxrtc | after M2 | oxrtc on the same scenarios, interleaved A/B with go2rtc |
| C - MediaMTX | after B | MediaMTX on the scenarios it supports (no native transcode: FFmpeg via `runOnDemand`) |
| D - In situ | after C | each server under openIVR itself, with its real viewers, recorder and AI |
| E - Decision | after D | the report, with the rows each server loses |

## Rules (from openIVR's performance campaign)

- Same machine, same binaries of FFmpeg, same media, same minutes: runs are
  **interleaved** (A B A B…), never "baseline this morning, candidate tonight".
  The development machine is shared; background load is read before each run
  and recorded with it.
- At least 3 runs per variant; report means with 95 % confidence intervals; a
  difference inside the interval is "no difference".
- Never conclude on a quantized median (the live bench's lag was quantized in
  70 ms steps).
- Results are raw JSON with versions, configs and machine facts; the summary is
  generated from them, never hand-edited.
- Publish the rows where oxrtc loses.

## Machines

| Name | What | Why |
|---|---|---|
| DEV | Windows 11, the owner's PC | where openIVR runs today |
| PI5 | Raspberry Pi 5, Linux arm64, Docker | the reference deployment; no H.264 encoder in hardware |
| LNX | a Linux amd64 box (optional) | cgroup-based measurement, `perf` |

## Sources

1. **Synthetic cameras**: `ffmpeg` publishing `testsrc2` with a 12-bit frame
   code burnt into the picture (12 squares of 36 px, top left), audio beeps
   synchronous with a flash; H.264 and H.265, 1080p and 2304×1296, GOP 1 s and
   4.83 s (the owner's cameras: 100/7 fps, GOP 69), plus a 512×288 substream.
   Served by a separate RTSP server process so the camera side is identical for
   every contender.
2. **Replayed recordings** of the owner's cameras, re-encoded with the frame
   code (local only, never committed - they show a home).
3. **The real cameras** (Jooan JA-K12 / XM), for phase D only, gently: they
   ration connections.

## Scenarios

| ID | Sources | Viewers | Measures |
|---|---|---|---|
| B01 | 1 H.264 | 1 WebRTC | CPU, RSS, first frame, latency |
| B02 | 1 H.265 | 1 WebRTC through the H.264 transcode | the same, transcode cost |
| B03 | 10 H.265 + substreams | 10 WebRTC (H.264 transcode) + 10 RTSP readers + 10 preloads | per-stream CPU and RSS - **the reference scenario** (openIVR's shape) |
| B04 | 25 | 50 | scaling |
| B05 | 50 | 100 | scaling |
| B06 | 1 | 50 viewers of the same camera | fan-out cost |
| B07 | 10 H.265 + substreams, G.711 sound at 16 kHz | openIVR's exact layout (`camera_N_src`, `_sub`, `_rec`, `camera_N`): 10 WebRTC with an audio transceiver + 10 recorders on `_rec` + 10 RTSP readers on `_sub` + 10 holds of `_rec` (`audio=opus`, then `video=`) | what B03 leaves out: the relays, the sound transcodes; go2rtc and oxrtc only |
| L01 | 1 H.264 | 1 browser | glass-to-glass latency, from the frame code |
| L02 | 1, held by a preload | 1 new viewer, ×20 | time to first frame, warm |
| L03 | 1, cold | 1 new viewer, ×20 | time to first frame, cold |
| R01 | 1 | 1 | camera killed for 10 s, 60 s, 30 min: time to recover |
| R02 | 10 | 10 | all cameras drop together: reconnection storm |
| R03 | 1 | 1 slow WebRTC viewer (netem 2 % loss, 300 ms) | the other viewers unaffected |
| S01 | 10 | 10 | 24 h soak: RSS, FDs, tasks, frames dropped |
| X01 | - | hostile | malformed RTSP, SDP floods, connection floods: survives, bounded |

No browser viewer receives H.265: Edge 157 offers no H.265 in WebRTC, headless
or not (measured 2026-11-14). An H.265 camera is watched through the H.264
transcode, as openIVR's live view does, so B02 and B03 measure that path.

MediaMTX does not transcode: B02 and B03 run it with
FFmpeg spawned by `runOnDemand`, which is how a NVR would deploy it. That
difference is reported, not hidden.

## Measurements

- **CPU**: process tree CPU time per second of run (server + its FFmpeg
  children), normalized per core; on Linux also cgroup `cpu.stat`
  (`nr_throttled` first - see the Pi 5 lesson).
- **Memory**: RSS and private bytes of the tree, sampled each second; growth
  over the soak.
- **First frame**: from the WHEP POST to the first decoded frame
  (`requestVideoFrameCallback`), in a headless Edge/Chromium.
- **Glass-to-glass**: the frame code read in the browser against the emitter's
  clock (calibrated once per publisher start), per frame; mean and P95.
- **Delivery**: `getStats()` each second - frames decoded/dropped, jitter
  buffer delay, packets lost; RTSP readers count frames with `ffprobe`.
- **Recovery**: time from the source's return to the next frame in the browser.

## Harness (`bench/`)

```
bench/
  cams/            synthetic camera publishers (ffmpeg + frame code), media builder
  servers/         one adapter per contender: config generator, start, stop, health
    go2rtc.py  mediamtx.py  oxrtc.py  rust2rtc_gpl.py
  clients/         WHEP browser probe (Playwright), RTSP readers, preload driver
  scenarios/       one file per scenario ID, declarative
  sample.py        process-tree CPU/RSS sampler (psutil)
  run.py           interleaved runner: scenario × server × repetitions
  report.py        JSON → markdown tables with confidence intervals
  results/         raw output (git-ignored)
```

It reuses what the live bench of 2026-11-09 proved: one publisher process per
camera emitting main and substream together (two processes start apart and skew
every comparison), the calibration of the emitter's clock, the frame-code reader
in the page, and killing processes through `psutil` (a Git Bash `kill` misses the
real FFmpeg behind the Chocolatey shim).

## The decision

The report ends with a recommendation for openIVR, one of:

1. stay on go2rtc (oxrtc does not earn its maintenance cost);
2. switch to oxrtc (it wins the reference scenario and loses nowhere that
   matters to a NVR);
3. switch to MediaMTX plus FFmpeg (it wins, and the transcoding gap is worth
   closing in openIVR);

with the scenarios that decided it. Security is scored on the threat-model
table (controls present and tested), not on language.

## Session of 2026-11-17 (phase B and C, final)

go2rtc 1.9.15, oxrtc 0.1.0 (release build of `v0.1.0`) and MediaMTX 1.21.3 on the owner's Windows PC, one at a time, interleaved A B B A, three repetitions, full durations:

- **Real cameras** (`bench/results/final-real-2026-11-17`): RC1-RC4 on the owner's six cameras with openIVR's exact layout (`bench/scenarios/RC1.yaml` to `RC4.yaml`, source URLs read from openIVR's config at run time, never stored).
- **Synthetic** (`bench/results/final-synthetic-2026-11-17`): L01, R01, R02, B04, B05, B06. The ten runs scheduled last failed the first time because the session driving them expired (no process could be started); they were run again the same morning and the failed attempts are in `_failed-login-expired/`, not counted.

The two-way tables are generated by `bench/tables.py` and published in openIVR's `results_bench/RESULTS.md`. Decision: **option 2, switch to oxrtc**. It wins the reference rows (L01 55 ms against 435 ms for go2rtc and 106 ms for MediaMTX: measured while oxrtc's answer named a stream per track, which left the browser's lip sync off for oxrtc alone; since W1 names one stream the row is a tie with MediaMTX, 102 against 140 ms, roadmap M4 and D30; warm first frame of the XM cameras 0.2 s against 5.3-5.6 s; the server process 11 % of one core against 98 % with openIVR's full layout) and loses five rows that do not decide an NVR, listed in the roadmap as R46. Not run: S01 (24 h soak, nine days for three servers), R03 (needs Linux), X01 (not implemented).

## The rule (D29, 2026-11-17)

**Better than go2rtc and MediaMTX on every row.** A domain is claimed in the
README only when none of its rows is lost or tied behind; a lost row is a work
item ([ROADMAP.md](ROADMAP.md), M4), never a footnote. Each session's
RESULTS.md opens with its scoreboard (won, tied, tied behind, lost).

Every protocol added by M5 to M10 enters the bench with its own scenarios,
against whichever reference speaks it, before the milestone closes:

| Family | Scenarios (to write with the feature) | Against |
|---|---|---|
| H | HLS and Low-Latency HLS: glass-to-glass latency, first frame, CPU and memory per reader, 100 readers | MediaMTX (LL-HLS), go2rtc (HLS) |
| M | MSE and MP4 over HTTP: first frame, latency, CPU | go2rtc |
| J | MJPEG and snapshots: `frame.jpeg` time, MJPEG stream CPU | go2rtc |
| P | Publishing: RTMP, WHIP and SRT ingest to a WebRTC and an RTSP reader (latency, CPU) | both |
| D | Recording to disk: CPU, memory, bytes written against bytes received | MediaMTX |
| O | The process itself: start-up time, binary size, memory with 0, 10 and 100 idle streams, 1,000 RTSP readers of one stream | both |

The other servers of the field ([PARITY.md](PARITY.md), "The field") join a
session when they run on the bench's machine and the owner has agreed to the
download of their binaries.

