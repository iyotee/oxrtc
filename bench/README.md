# bench - the media-server benchmark harness

The harness behind [docs/BENCHMARKS.md](../docs/BENCHMARKS.md): synthetic cameras,
one adapter per contender, browser and RTSP clients, a process-tree sampler, an
interleaved runner and a report generator. Read the plan first; this file says
how to run it and what each piece does.

## Requirements

- Python 3.12 with `psutil` and `PyYAML` (standard library otherwise).
- FFmpeg and ffprobe - `OXRTC_FFMPEG` / `OXRTC_FFPROBE`, default the Chocolatey
  *real* binaries under `C:/ProgramData/chocolatey/lib/ffmpeg/tools/ffmpeg/bin/`
  (not the shim: killing a shim leaves the real FFmpeg running).
- go2rtc - `OXRTC_GO2RTC` (default `../openIVR/bin/go2rtc.exe`, the sibling checkout). It is the
  camera server in every run, and the first contender.
- Node.js and Playwright for the browser probe. Playwright is not installed by
  the bench: it is loaded from openIVR's frontend (`OXRTC_PLAYWRIGHT_FROM`,
  default `../openIVR/frontend/package.json`) and drives
  the installed Microsoft Edge (`channel: msedge`; `OXRTC_BROWSER_CHANNEL`).
- Contenders: `OXRTC_BIN` (oxrtc, default `target/release/oxrtc(.exe)`),
  `OXRTC_MEDIAMTX`, `OXRTC_RUST2RTC_GPL`. An absent binary is skipped and the
  skip is written into the session.

## Run

From the repository root:

```sh
python bench/run.py --scenarios B01 --servers go2rtc,oxrtc --reps 3
python bench/report.py bench/results/<session> --out bench/results/<session>/report.md
```

`--scenarios all` runs every file in `scenarios/`; `--order abba` alternates the
contender order per repetition; `--duration 20` shortens every scenario (smoke
runs only - never report a shortened run). Unit tests:

```sh
python -m unittest discover -s bench/tests
```

## Layout

| Path | What |
|---|---|
| `cams/media.py` | builds each camera profile's media once (cached in `bench/media/`, git-ignored): `testsrc2` with the 12-bit frame code, a flash and a beep each second, main **and** substream in one Matroska file, exactly 4096 frames so a loop does not break the code |
| `cams/publisher.py` | the camera server (a go2rtc of its own on free ports, distinct from the server under test) and ONE FFmpeg per camera pushing main + substream from one demuxer (`-c copy`, real-time, looping); `calibrate()` reads the emitter's clock off the camera server |
| `servers/*.py` | one adapter per contender: config generator, start, stop, health, WHEP/RTSP URLs, preload |
| `clients/whep_probe.mjs` | headless Edge viewers: WHEP offer/answer, frame code read per presented frame (`requestVideoFrameCallback`), `getStats()` each second, first frame decomposed (answer, ICE, track, frame) |
| `clients/framecode.js` | the probe's decoder - the JavaScript twin of `framecode.py`, checked against it by the tests |
| `clients/rtsp_reader.py` | ffprobe readers counting received packets (or frames) over TCP |
| `clients/preload.py` | holds a stream open: the server's preload API, or an RTSP reader where there is none |
| `scenarios/*.yaml` | one declarative file per scenario ID (format in `scenario.py`) |
| `sample.py` | process-tree CPU, RSS, private bytes, threads and handles every second; background load before each run |
| `run.py` | the interleaved runner; one raw JSON per run in `bench/results/<session>/` |
| `report.py` | raw JSON to markdown: per-run values, mean ± 95 % CI across runs, "no difference" when intervals overlap |

## Scenarios

| ID | Status | What it measures |
|---|---|---|
| B01 | ready | 1 H.264 camera, 1 WebRTC viewer: tree CPU, RSS, first frame, latency |
| B02 | ready | 1 H.265 camera, a viewer through the H.264 transcode: transcode cost (Edge offers no H.265 in WebRTC) |
| B03 | ready | **reference**: 10 H.265 cameras (owner's profile, 100/7 fps, GOP 69) with substreams, 10 WebRTC through the H.264 transcode, 10 RTSP readers on the substreams, 10 preloads |
| B04 | ready | 25 cameras, 50 viewers: scaling |
| B05 | ready | 50 cameras, 100 viewers: scaling (client load is heavy, read the machine CPU) |
| B06 | ready | 1 camera, 50 viewers: fan-out cost |
| L01 | ready | glass-to-glass latency per frame, mean and P95 |
| L02 | ready | warm first frame: preload held, a new viewer 20 times, GOP 69 |
| L03 | ready | cold first frame: nothing held, 20 viewers with an 8 s gap |
| R01 | ready | camera down 10 s then 60 s: time from its return to the next frame |
| R02 | ready | 10 cameras down together: reconnection storm |
| R03 | needs-linux | one viewer behind netem; skipped off Linux |
| S01 | ready | 24 h soak: RSS, handles, threads, delivery |
| X01 | not-implemented | hostile clients: needs an attacker the bench does not have yet |

## How the measurements work

- **Frame code.** Square `b` of 12 (36 px each at the main stream's width,
  top left) is white when bit `b` of the frame number is set. The reader scales
  the square size by `video width / reference width`, so the substream and the
  browser's picture read the same code. Codes that do not follow their
  predecessor by 0-5 frames are rejected (garbage before a keyframe), unless
  the next two codes follow them: then the reader re-anchors (a freeze or a
  skip of more than 5 frames would otherwise reject the rest of the run).
- **Emitter clock.** Frame `n` leaves the camera server at `E0 + n x frame`.
  At publisher start a single-threaded FFmpeg reads the substream off the
  camera server; the smallest `arrival - n x frame` is taken as E0, minus one
  frame interval: FFmpeg's parser hands an access unit over only when the next
  one starts (measured: the uncorrected offset scaled with the frame interval,
  28 ms at 25 fps and 56 ms at 100/7 fps). Everything is modulo the code's wrap
  (4096 frames), so no unwrapping is needed.
- **Latency** of a presented frame = its wall time in the browser minus its
  emission. The browser clock (`performance.timeOrigin + now`) is checked
  against `Date.now()` at start and end (`clock_skew_ms`).
- **CPU** is the cumulative user+system time of the server's process tree
  (its FFmpeg children included), per second, as % of one core and of the
  machine. A child that lives less than one sampling interval is missed.
- **First frame** = from the WHEP POST to the first presented frame.
- **Cycles** (L02, L03): each offer waits `spread_ms x frac(i x 0.618)` first
  (`clients/browser.py` `cycle_delays`), one GOP wide, the same sequence for
  every server. A cycle otherwise lasts first frame + hold + gap, so its length
  depends on the server and every offer lands at the same phase of the GOP:
  in phase A, go2rtc's L03 cycle was two GOPs to within 30 ms and its "cold
  start" read 515 ms flat, a keyframe always 400 ms away.
- **Reconnect** (R01, R02): a lost session is replaced as openIVR's live view
  does (`frontend/src/stores/streaming.ts`: failed or closed at once,
  disconnected after 5 s, retried after 1 s, 2 s, 4 s ... 30 s). MediaMTX
  closes its viewers' sessions when the source drops; go2rtc and oxrtc keep
  them, so for those two nothing changes.

## Rules

- **Interleave.** Never "all of A, then all of B". The runner orders runs
  repetition-major with contenders interleaved inside each repetition.
- **Idle machine.** Real runs happen with the machine idle; the background
  load is recorded before every run and the machine CPU beside the server's
  during it. A difference that follows the background is not a result.
- **At least 3 repetitions**; means with 95 % confidence intervals; overlap
  means "no difference". One repetition gives no interval and no verdict.
- **Never a median.** Latency is reported as mean and P95.
- **Raw JSON is the result.** `bench/results/` is git-ignored; the markdown is
  regenerated from it, never edited. Record versions (the runner does).
- **Publish the rows where oxrtc loses.**
- **Never touch the owner's processes or ports.** Every port is free and
  random (1984, 8554, 8555, 7998-8000, 11984, 18554, 18555, 18998, 21984 and
  28554 are refused); every process is started from an argument vector and
  stopped through its own handle with its descendants, never by name. On
  Windows each child is put in a job object that kills its whole tree when the
  job closes - on `stop()`, or when the runner dies, however it dies
  (`tests/test_processes.py` kills a runner abruptly and checks its FFmpeg is
  gone). Without a job, `stop()` walks the tree from our pid with psutil, and
  only while that pid is still the process it started (same creation time).
- **Nothing identifying in tracked files or results**: no camera address, no
  password, no account name. ICE candidates are summarized (protocol, type,
  loopback or not) - the raw lines carry the machine's addresses. The oxrtc
  adapter's per-run API password is redacted from the recorded config (it
  is random, and still present in the run folder's `server/oxrtc.yaml` and in
  `clients/probe-job.json` as an HTTP Basic header). The
  servers' own logs (kept beside each run's JSON) can name the machine's
  interface addresses: `bench/results/` is git-ignored, and a run folder is
  never published as is - publish the generated report.

## Known limits

- The server under test's WebRTC port is bound on every interface (as openIVR
  deploys go2rtc): bound to 127.0.0.1 alone, ICE never completed with go2rtc
  1.9.15 and Edge 157. go2rtc's default public STUN server is disabled - it
  added 5 s to every WHEP answer.
- With the camera's PCMA track, go2rtc's B01 latency is 419 ms in phase A
  (217 ms in an early smoke run, about 30 ms without audio). oxrtc sends the
  same PCMA and H.264 tracks and reads about 30 ms, so the browser's lip sync
  alone does not explain it; the cause is not established yet. It is part of
  what a viewer sees, and is measured, not removed.
- The GPL rust2rtc adapter is written from the assumption that it is
  go2rtc-compatible and has never run; it says so in its module docstring and
  in every run's JSON (`tested_adapter`). oxrtc's has run every ready
  scenario (2026-11-14), MediaMTX's too, against v1.21.3 (2026-11-15).
- MediaMTX's transcode is an FFmpeg command line the bench writes
  (`servers/mediamtx.py`), not go2rtc's template: its B02/B03 figures measure
  that recipe, through one more RTSP hop.
- After a camera outage the publisher restarts its file at frame 0: latency is
  not computed across a fault (R01/R02 do not decode the code). A fault that
  would end after the measured window (a short `--duration`) is recorded as
  skipped in the run's `faults`, never played after the teardown.
- Off Windows there is no job object: a runner killed abruptly (SIGKILL)
  leaves its children running: find those whose command line names the run's
  folder or `bench/media/`, and stop them by pid, never by name.
