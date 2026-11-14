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
