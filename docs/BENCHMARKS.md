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
- Six runs per contender, interleaved A B B A, for a session that judges a row
  against the references; three for oxrtc against itself ("The rule" below);
  report means with 95 % confidence intervals; a difference inside the
  interval is "no difference".
- Never conclude on a quantized median (the live bench's lag was quantized in
  70 ms steps).
- Results are raw JSON with versions, configs and machine facts; the summary is
  generated from them, never hand-edited.
- Publish the rows where oxrtc loses.
- A run mean charges a process for the time it ran. A contender whose
  transcodes start late is charged less CPU and memory than one that shows
  its picture at once (RC4, 2026-11-20: go2rtc's webcam transcodes show their
  first picture 39 to 46 s into a 120 s run, oxrtc's 6 to 9 s, and the
  run-mean FFmpeg rows read 174 % of a core against 220 where the second
  minute reads 235 against 221). Such a row is still published as lost;
  beside it the tables give the same quantity over equal work (W6,
  `bench/work.py`): the CPU per delivered frame and a camera's FFmpeg CPU per
  picture its viewer decoded, and the tree's CPU and memory while every
  transcode runs. On W4's acceptance session (RC4, 3 repetitions) the two
  webcams' FFmpeg CPU per picture reads 30.9 +/- 0.5 and 24.3 +/- 1.5 ms of a
  core against go2rtc's 25.2 +/- 7.0 and 24.0 +/- 3.3 (no difference), the
  children's CPU while every transcode runs 232 against 233 % of a core (no
  difference), and the children's memory then 588 against 543 MiB (lost; that
  build still ran an FFmpeg per camera with sound, W5 removed them).
- **Equal work** (W6). Every contender does the same work, each through its
  own means: the same pictures to every consumer at the camera's rate, the same
  tracks, the same transcodes. Where one cannot, its row says unsupported with
  the reason. A transcode's encoding level is not the bench's to set (go2rtc's
  depends on FFmpeg's own rate guess, D36): the bitrate each viewer received is
  in the tables beside the CPU it cost. On the real cameras the contenders
  are compared on the same cameras: a session measures the set of cameras
  its first run measured (the ones that answered), ends a scenario when the
  owner disables one of them, and makes a run where one of them does not
  answer an error, not a repetition (bench/README.md, "Real cameras").

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
| RC5 | the owner's real cameras, openIVR's layout | 5 browser processes, each with openIVR's live grid (a tile per enabled camera on `camera_N`), the recorder and its hold as in RC1 | **the real case** (D29 as amended): delivery, freezes, first frame, CPU and RSS split server / children, CPU per delivered frame; run in both oxrtc modes |
| L01 | 1 H.264 | 1 browser | glass-to-glass latency, from the frame code; since W1b also the sound the browser holds, how long it holds the picture for the sound, and how far the sound is behind the picture, twice: by the servers' dates (the two tracks' `estimatedPlayoutTimestamp`) and as the viewer meets them (each beep heard, through Web Audio, minus its flash seen; a constant of the measuring path, about 80 ms, is in every contender's figure alike, so it is compared between contenders, not read as an absolute). All three servers name one stream, so the browser synchronises the picture with the sound for each of them (go2rtc, MediaMTX, oxrtc by default); `OXRTC_BENCH_LIP_SYNC=0` runs oxrtc with `webrtc.lip_sync: false`, which the browser does not synchronise (D30). oxrtc dates its sound `webrtc.sound_lag_ms` (20 ms) after its picture since W1c, which the second figure shows and the first cannot (it is built from those dates: add the lag to it); `OXRTC_BENCH_SOUND_LAG_MS=<ms>` runs oxrtc with another value, for a comparison (`0`: dated when it arrived), never for an acceptance run |
| L02 | 1, held by a preload | 1 new viewer, ×20 | time to first frame, warm |
| L03 | 1, cold | 1 new viewer, ×20 | time to first frame, cold |
| R01 | 1 | 1 | camera killed for 10 s, 60 s, 30 min: time from its return to its first NEW picture in the browser (the frame code read on every viewer: a frozen keyframe a server replays on the frozen browser's keyframe request is not a recovery, `run.recovery_ms`, since 2026-11-18) |
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

The two-way tables are generated by `bench/tables.py` and published in openIVR's `results_bench/RESULTS.md`. Decision: **option 2, switch to oxrtc**. It wins the reference rows (L01 55 ms against 435 ms for go2rtc and 106 ms for MediaMTX: measured while oxrtc's answer named a stream per track, which left the browser's lip sync off for oxrtc alone; since W1 names one stream the row is a tie with MediaMTX, 102 against 140 ms, and still one since W1b with lip sync on, 82.4 +/- 25.8 against 110.0 +/- 16.3, oxrtc the lower in each repetition; with `webrtc.lip_sync: false` it is won, 8.3 against 117.3 ms; roadmap M4 and D30; warm first frame of the XM cameras 0.2 s against 5.3-5.6 s; the server process 11 % of one core against 98 % with openIVR's full layout) and loses five rows that do not decide an NVR, listed in the roadmap as R46. Not run: S01 (24 h soak, nine days for three servers), R03 (needs Linux), X01 (not implemented).

## Session of 2026-11-23 (round 1, the full bench again)

go2rtc 1.9.15, oxrtc 0.2.0 (release build of `fc106eb`, pinned for the round) and MediaMTX 1.21.3 on the owner's Windows PC with his NVR running, one at a time, interleaved A B B A, **six repetitions**, full durations; each run took the lanes' bench lock (no compiler and no other measured job beside it):

- **Real cameras** (`bench/results/real-2026-11-23-r1`): RC1-RC5 on the owner's six cameras (all six answered), RC5 being the real case (five Edge processes on openIVR's live grid, W6).
- **Synthetic** (`bench/results/synthetic-2026-11-23-r1`): B01-B07, L01-L03, R01, R02 (MediaMTX skipped in B07: it cannot express openIVR's layout). B02 and B03 were run again after `66eef36` (MediaMTX's synthetic transcode lacked openIVR's `-fps_mode passthrough` and repeated pictures); the first runs are in `_mediamtx-repeated-pictures-B02/` and `-B03/`, not counted.
- **Live mode** (`OXRTC_BENCH_LIP_SYNC=0`): `real-live-2026-11-23-r1` (RC5) and `synthetic-live-2026-11-23-r1` (L01, B07), for each mode against its peer (`bench/peers.py`).
- Two crashes (18:57, 22:13) and a reboot (23:40) cut a run each time; the cut scenario was run again whole, the partial runs kept in `_partial-*/`. After the reboots the owner's NVR used about half a core more: the B03 rerun saturated the machine for every contender (latency 9.5-13 s for all three), and the live-mode B07 is not comparable with the default one.

Scoreboard (`bench/scoreboard.py`), the D29 count: **352 rows, 106 won, 185 tied, 4 tied behind, 16 lost, 41 without verdict**; stress rows (B05, B06) 26: 14, 4, 2, 2, 4; the live-mode sessions apart, 106 rows: 27, 57, 2, 5, 15. L01 with lip sync on is won on both rows (79.3 +/- 3.7 ms against MediaMTX's 115.5 +/- 5.6, P95 105.8 against 136.6; go2rtc 480.3), which ticks W1b and W1c. The lost rows: the webcams' and the fan-out's FFmpeg CPU (RC1, RC4; oxrtc's encodes send 2 to 10 times go2rtc's bitrate), RC5's longest gap on camera_1 and frames dropped by the browser (the XM cameras' late keyframes), B03's and B07's FFmpeg memory and B07's tree CPU, B07's frames dropped, and L03's cold first frame; the list with the three values is in [PARITY.md](PARITY.md), "The bench", and the tables in openIVR's `results_bench/RESULTS.md` (the 2026-11-17 session moved to `results_bench/history/2026-11-17/`). Not run: S01 (24 h soak, 18 days at six repetitions), R03 (needs Linux), X01 (not implemented).

## The rule (D29, 2026-11-17)

**Better than go2rtc and MediaMTX on every row.** A domain is claimed in the
README only when none of its rows is lost or tied behind; a lost row is a work
item ([ROADMAP.md](ROADMAP.md), M4), never a footnote. Each session's
RESULTS.md opens with its scoreboard (won, tied, tied behind, lost).

**The scoreboard** (`bench/scoreboard.py`, printed by `bench/tables.py` at the
top of what it writes; W6). Each row of a session's `results.csv` is one of:
**won** (oxrtc's interval clear of both others', on the better side), **tied**
(intervals overlap and oxrtc is among the best), **tied behind** (the best
overlap each other, oxrtc is clear of them on the worse side), **lost**
(another is better, clear of oxrtc), **without verdict** (an information row,
one run, fewer than two contenders, or oxrtc without a value). Three counts are
printed: the rows D29 counts, the stress rows apart (B05, B06), and all rows;
then every lost and tied-behind row with the three values. Every row names the
oxrtc mode its runs used (`oxrtc mode` in the CSV): lip sync on (the default),
live (`webrtc.lip_sync: false`), or 0.1.x, whose stream per track left the
browser's lip sync off. The session of 2026-11-17 reads, all rows: 28 won, 98
tied, 6 tied behind, 15 lost, 13 without verdict (160); the D29 count alone 20,
92, 5, 14 and 11 (`bench/tests/test_scoreboard.py` pins both on a copy of that
file).

**Repetitions (the owner, 2026-11-20: "6 passes pour tout le banc", six
repetitions for the whole bench; and 2026-11-21).** Every session that judges a
row against go2rtc or MediaMTX runs `--reps 6 --order abba` (the defaults of
`bench/run.py`): the full bench, an item's acceptance, the real case. A row is
judged on that session alone and never pooled with earlier sessions. Why: on
L01 with lip sync on oxrtc had the lower latency in 8 runs of 9, pooled 81.3
+/- 11.6 ms against MediaMTX's 107.2 +/- 7.8, and yet tied in 2 sessions of 3
at three repetitions (W1c review). A session that compares oxrtc with oxrtc
(native audio against FFmpeg's sound, a change against the build before it, a
key on against off, a pair session of `pair.py`) runs `--reps 3`.

**Each mode against its peer (the owner, 2026-11-20; D30).** A latency row
against go2rtc compares oxrtc in live mode (`webrtc.lip_sync: false`,
`OXRTC_BENCH_LIP_SYNC=0`), since go2rtc does not hold its picture for the
sound in openIVR's layout; a lip-sync row compares oxrtc's default against the
server that synchronises (MediaMTX). Every scoreboard row names the mode.

**Stress rows (D29 as amended by the owner, 2026-11-19).** The deployment is
bare metal in a home or a small site, two to five viewers at most. B05 (100
viewers in one browser) and B06 (50) are stress rows: run, reported and never
made worse, but outside the count, and never won by a server-side trick that
costs latency or lip sync in the real case. The real case to win is up to five
viewers, each in a browser process of its own, on openIVR's live grid (W6).

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

