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
| `crates/ffmpeg` | Transcoding and stills through an FFmpeg subprocess, confined on Linux. |
| `crates/api` | HTTP API: the go2rtc-compatible surface and the native one. |
| `apps/oxrtc` | The binary. |
| `apps/oxrtc-exec` | FFmpeg's confinement on Linux: dies with oxrtc, no new privileges, limits, Landlock; shipped beside `oxrtc`. |
| `bench/` | The comparison bench: go2rtc, MediaMTX, oxrtc. |
| `fuzz/` | cargo-fuzz targets for the parsers and packetizers (nightly; their proptest twins run in `cargo test`). See [fuzz/README.md](fuzz/README.md). |
| `scripts/` | `release_local.py`: the release assets (build, checks of each binary, SBOMs, third-party notices, archives, `SHA256SUMS`), on a developer's machine or in `release.yml`; its tests in `scripts/tests`. |
| `.github/workflows/` | `ci.yml` (fmt, clippy, tests on Linux and Windows, the scripts' tests, `cargo deny`, `cargo audit`), `release.yml` (`scripts/release_local.py` per platform, then `SHA256SUMS`), `fuzz.yml` (on demand). |
| `deny.toml` | Dependency policy: permissive licences only, advisories, crates.io only. |

## Installing

A release is three archives and their checksums: `oxrtc-<version>-linux-amd64.tar.gz`,
`oxrtc-<version>-linux-arm64.tar.gz` (Raspberry Pi 5, and any 64-bit ARM
Linux) and `oxrtc-<version>-windows-amd64.zip`, a CycloneDX SBOM per binary
beside them (`*.cdx.json`, also in the archive's `sbom/`), and `SHA256SUMS`.
The Linux binaries need glibc 2.35 or newer (Ubuntu 22.04, Debian 12 /
Raspberry Pi OS Bookworm) and nothing else from the system: no OpenSSL.

```sh
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf oxrtc-<version>-linux-arm64.tar.gz
cd oxrtc-<version>-linux-arm64
./oxrtc --version
```

On Linux keep **`oxrtc-exec` beside `oxrtc`**, in the same directory: FFmpeg
starts through it, confined and killed with oxrtc (without it oxrtc warns and
runs FFmpeg unconfined - [COMPATIBILITY.md](docs/COMPATIBILITY.md)). FFmpeg
itself is not in the archive: the distribution's, or a static build, on the
`PATH` or named by `ffmpeg.bin`. Each archive carries `THIRD_PARTY_NOTICES.txt`,
the licences of every crate compiled in.

The assets build on one Windows or Linux machine, no administrator rights
needed, the two Linux targets cross-compiled with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) from Windows
(the tools and their versions are at the top of the script):

```sh
python scripts/release_local.py --out ../oxrtc-release        # the three platforms
python scripts/release_local.py --platform linux-arm64 --out ../oxrtc-release
```

`.github/workflows/release.yml` runs the same script, one platform per job,
on a `v*` tag.

## Running

```sh
cargo build --release
target/release/oxrtc -config oxrtc.yaml     # go2rtc's flag; --config works too
target/release/oxrtc --version
```

The configuration is the subset of go2rtc's YAML described in
[COMPATIBILITY.md](docs/COMPATIBILITY.md). A minimal one:

```yaml
api:
  listen: "127.0.0.1:1984"
  username: "admin"          # required: the API is never open by default
  password: "change-me"
rtsp:
  listen: "127.0.0.1:8554"
webrtc:
  listen: ":8555"
streams:
  camera_src: ["rtsp://user:pass@192.168.1.100:554/stream1#timeout=15"]
  camera: ["rtsp://127.0.0.1:8554/camera_src?video", "ffmpeg:camera_src#video=h264#width=1280#height=720"]
log:
  level: info
```

Logs go to stdout as `HH:MM:SS.mmm LVL [module] message key=value`. FFmpeg is
`ffmpeg.bin`, else `$OXRTC_FFMPEG`, else `ffmpeg` from the `PATH`. Ctrl+C or
SIGTERM stops the server (exit code 0; on Windows Ctrl+Break and closing the
console too, and a hard kill takes the FFmpeg processes with it through their
job objects). A port already in
use or an unusable configuration exits with 1 and says why; a usage error with 2.

## Documents

- [Architecture](docs/ARCHITECTURE.md) and [decisions](docs/DECISIONS.md)
- [Compatibility with go2rtc](docs/COMPATIBILITY.md) - the exact surface a NVR relies on
- [Threat model](docs/THREAT_MODEL.md)
- [Roadmap](docs/ROADMAP.md) and its acceptance criteria
- [Benchmark plan](docs/BENCHMARKS.md)

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
