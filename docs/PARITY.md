# Parity and beyond

What go2rtc and MediaMTX do, what the other servers of the field do, where
oxrtc stands on each line today, and the milestone that closes each gap.
The goal (owner, 2026-11-17): **oxrtc is the union of the two, in Rust, and
measures better than both on every row of the bench**. This file is the list
that goal is checked against; [ROADMAP.md](ROADMAP.md) holds the work items
(M4 to M10) and [BENCHMARKS.md](BENCHMARKS.md) the measurements.

Read from the sources, not from memory: go2rtc at `990f4de` (2026-11-05, version
1.9.15, MIT) and MediaMTX at `ed7d6ea` (2026-11-17, MIT), their READMEs, module
documents and reference configuration. Nothing of their code is copied: a
behaviour is re-implemented from the protocol's specification and from what
the reference server answers on the wire.

**Status words.** `done`: in a release, with a test. `partial`: works for the
case named. `M4` ... `M10`: planned in that milestone. `never`: refused, with
the reason. A row is closed only by `done` or `never`.

## The field

| Server | Language | What it is best at | What oxrtc takes from it |
|---|---|---|---|
| go2rtc | Go | cameras: dozens of sources (RTSP, RTMP, ONVIF, HomeKit, vendor protocols), every browser format (WebRTC, MSE, HLS, MJPEG), two-way audio, one stream from several sources, Home Assistant | its whole camera-facing surface and its HTTP/WebSocket API, so that everything built for go2rtc works unchanged |
| MediaMTX | Go | a media router: publish and read in every protocol (RTSP with UDP and multicast, RTMP, SRT, WebRTC WHIP/WHEP, LL-HLS, Media over QUIC), recording and playback, authentication per path, metrics, hooks, hot reload | its publish side, its operations features and its protocol depth |
| SRS | C++ | RTMP/HTTP-FLV/HLS at scale, WebRTC, SRT, origin and edge clusters, DVR, GB28181 | HTTP-FLV, the cluster idea (read replicas), DVR semantics |
| ZLMediaKit | C++ | the widest protocol matrix of the Chinese surveillance field (GB28181, RTP proxy), per-event web hooks, MP4 on demand | web hooks as the safe form of hooks; GB28181 is listed, not planned |
| OvenMediaEngine | C++ | sub-second delivery at scale: WebRTC and LL-HLS with adaptive bitrate, built-in transcoding | LL-HLS as a first-class output; renditions (ABR) |
| Janus, mediasoup, LiveKit | C, C++/Node, Go | selective forwarding for conferences (simulcast, data channels, rooms) | nothing now: a conference SFU is another product; WHIP/WHEP is the common ground |
| Monibuca, lal | Go | plugin servers: RTMP, RTSP, HLS, HTTP-FLV, WebRTC, GB28181 | nothing they have that the two references lack |
| RTSPtoWeb, webrtc-streamer | Go, C++ | the smallest RTSP-to-browser bridges (WebRTC, MSE, HLS) | the proof that the bridge alone is a product: oxrtc's default run must be as simple |
| xiu, live777 | Rust | Rust servers: RTMP/HLS/HTTP-FLV/WHIP/WHEP (xiu), a WHIP/WHEP SFU with RTP and RTSP bridges (live777) | the Rust comparison point for the bench (same language, same allocator questions) |
| neolink, Scrypted, Frigate | Rust, TS, Python | vendor bridges (Reolink Baichuan), HomeKit Secure Video, an NVR that embeds go2rtc | the vendor protocols users ask a gateway for; Frigate is a consumer of the go2rtc API |

What the field has that neither reference has, and where it goes: MPEG-DASH
(M5, with HLS: same fMP4 segments), HTTP-FLV in and out (M6, with RTMP),
adaptive renditions (M7, through the transcoder), read replicas (M7: a stream
can be another oxrtc), GB28181 (`never` for now: no user of this project has
it; reopened on the first request).

## Sources (the server pulls)

| Source | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| RTSP over TCP, Digest/Basic | yes | yes | done | - |
| RTSPS (TLS) | yes (`rtsps`, `rtspx`) | yes | done (`#verify`, `#insecure`); `rtspx` alias | M6 |
| RTSP over UDP, UDP multicast | no (TCP only) | yes | no | M6 |
| RTSP tunnelled (WebSocket, HTTP) | WebSocket | WebSocket, HTTP | no | M6 |
| RTSP backchannel (two-way audio, ONVIF Profile T) | yes | no | no | M6 |
| HTTP MJPEG (multipart), JPEG polled | yes | no (through FFmpeg) | done | - |
| HTTP/TCP raw streams: FLV, MPEG-TS, H.264/H.265 bitstream, ADTS, WAV, Y4M | yes | MPEG-TS (UDP), RTP (UDP) | no | M6 |
| HLS as a source | yes (poor, by its own word) | yes | no | M6 (through the transcoder first, native later) |
| RTMP pull | yes | yes | no | M6 |
| SRT pull | no | yes | no | M6 |
| WebRTC pull (WHEP, go2rtc WebSocket, vendor signalling: Kinesis, OpenIPC, Creality, SwitchBot, Wyze bridge) | yes | WHEP | no | M6 (WHEP, go2rtc WS), M8 (vendor) |
| Media over QUIC pull | no | yes | no | M10 |
| ONVIF (`onvif://`: profiles, stream URI, snapshot URI; discovery) | yes | no | no | M6 |
| FFmpeg: any input it reads (files, HLS, devices), transcoding, hardware encoders | yes | through hooks | partial: transcodes of a stream or an RTSP URL to H.264 and Opus, templates, `#input=`; no file, device, hardware, `#video=mjpeg`, `#raw` | M6 (R12) |
| A test pattern (`ffmpeg:virtual`) | yes | no | no | M6 |
| `exec:` (a program's output: pipe or RTSP) | yes | `runOnInit` / `runOnDemand` | never by default (SEC-007); **opt-in** with an allow-list of programs | M6 |
| `echo:` / `expr:` (a script or an expression that returns the URL) | yes | no | never by default; opt-in with `exec` | M6 |
| Local devices: V4L2, ALSA (Linux), DirectShow (Windows), Raspberry Pi camera | V4L2, ALSA native; others through FFmpeg/exec | Raspberry Pi camera native; others through FFmpeg | no | M6 through FFmpeg; native V4L2/ALSA M8 |
| HomeKit cameras | yes | no | no | M8 |
| Vendor protocols: Tapo/Vigi, Kasa, DVR-IP (XMeye), Bubble/ESeeCloud, GoPro, Doorbird, Hikvision ISAPI audio, TP-Link multitrans | yes | no | no | M8, one by one |
| Cloud camera families: Nest, Ring, Tuya, Xiaomi, Wyze (P2P), Roborock, Yandex, Ivideon, Flussonic | yes | no | no | M8, each decided on its own (accounts, terms of use, maintenance) |
| Cameras of a Home Assistant instance (`hass:`) | yes | no | no | M8 |
| Another stream of the same server | yes | yes (`sourceRedirect`, proxy) | done (`stream:`, loopback URLs as aliases) | - |
| Several sources for one stream, the codec chosen by what the reader accepts | yes | no (one source per path) | done | - |
| On-demand sources (opened for the first reader, closed after the last) | yes | yes (`sourceOnDemand`) | done | - |

## Publishing to the server (a client pushes)

| Ingest | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| RTSP `ANNOUNCE`/`RECORD` | yes | yes (TCP, UDP) | partial: the transcoders' own publications, on loopback | M6 |
| RTMP publish (OBS, cameras, GoPro), enhanced RTMP codecs | yes (H.264/AAC) | yes (AV1, VP9, H.265, H.264, Opus, AAC...) | no | M6 |
| HTTP-FLV, MPEG-TS, MJPEG `POST` | yes | no | no | M6 |
| WebRTC WHIP (OBS, browsers), browser camera/screen publish | yes | yes | no | M6 |
| SRT publish | no | yes | no | M6 |
| Media over QUIC publish | no | yes | no | M10 |

## Outputs (a client reads)

| Output | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| WebRTC, WHEP | yes | yes | done (session resource, trickle, ICE-TCP, TURN UDP/TCP) | - |
| WebRTC over the go2rtc WebSocket API (`/api/ws`) | yes | no | no | M5 |
| One MediaStream for the picture and the sound of a session | yes | yes | no (one per track; broke a `streams[0]` player on 2026-11-17) | M4 |
| RTSP (TCP interleaved), codec filters in the query | yes | yes | done | - |
| RTSP over UDP, multicast, RTSPS, several audio tracks, `default_query` | `default_query`, several tracks | UDP, multicast, RTSPS | no | M6 |
| MSE: fragmented MP4 over WebSocket | yes | no | no | M5 |
| MP4 over HTTP (progressive), one-frame MP4 (`frame.mp4`), timed clips (`duration=`) | yes | playback server only | no | M5 |
| HLS: MPEG-TS and fMP4 | yes (non-standard, by its own word) | yes | no | M5 |
| Low-Latency HLS | no | yes | no | M5 |
| MPEG-DASH | no | no | no | M5 (same segments as HLS) |
| MJPEG (multipart), JPEG snapshot (`width`, `height`, `rotate`, `cache`) | yes | no (FFmpeg hook) | partial: `frame.jpeg` | M5 |
| MPEG-TS over HTTP, ADTS, Y4M, FLV over HTTP | yes | no | no | M6 |
| RTMP play | yes | yes | no | M6 |
| SRT read | no | yes | no | M6 |
| Media over QUIC read | no | yes | no | M10 |
| HomeKit (cameras exported to Apple Home) | yes | no | no | M8 |
| ONVIF server (streams discoverable by an NVR) | yes | no | no | M6 |
| WebTorrent, ngrok and pinggy tunnels, ASCII art | yes | no | no | `never` for the third-party tunnels (a server that opens itself to the Internet through a company's relay is the opposite of this project's default); ASCII: `never` |

## Pushing elsewhere

| Feature | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| Publish a stream to RTMP/RTMPS services (YouTube, Twitch, Telegram) | yes | through FFmpeg hooks | no | M6 |
| Forward a stream to another server (RTSP, RTMP, SRT, WebRTC) | RTMP | yes | no | M7 |
| Play a file or a live stream on a camera's speaker (`POST /api/streams?dst=&src=`) | yes | no | no | M6 |
| Two-way audio from the browser's microphone | yes (WebRTC) | no | no | M6 |

## Codecs

| Codec | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| H.264, H.265 | yes | yes | done (RTSP, WebRTC) | - |
| M-JPEG | yes | RTSP, recording | done (HTTP in, RTP/JPEG out) | - |
| AV1, VP9, VP8 | no | yes | no | M8 |
| MPEG-4 Video, MPEG-1/2 Video | no | yes | no | M8 (pass-through only) |
| Opus, G.711 (PCMA, PCMU) | yes | yes | done | - |
| AAC (MPEG-4 audio) | yes | yes | partial: carried over RTSP | M5 (MP4, HLS), M6 (RTMP) |
| G.722, G.726, MP3, AC-3, LPCM, FLAC | MP3, PCM | yes | L16 carried | M8 |
| PCM family packed as FLAC for MP4/MSE/HLS | yes | FLAC in MoQ, RTMP, HLS | no | M5 |
| G.711 resampled to 8 kHz for WebRTC, PCM to PCMA | yes, in process | no | no (an FFmpeg per camera does it to Opus) | M4 (native audio, R33) |
| KLV and other data tracks | no | yes | no | M8 |

## API and integrations

| Surface | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| `GET /api`, `/api/streams` (GET, PUT, DELETE), `/api/preload`, `/api/frame.jpeg`, `POST /api/webrtc` | yes | - | done | - |
| `/api/streams` `PATCH` and `POST` (`src`/`dst`), `/api/streams.dot`, `/api/schemes` | yes | - | no | M5 |
| `/api/config` (GET, POST, PATCH), `/api/restart`, `/api/exit`, `/api/log` | yes | - | no | M5 |
| `/api/ws` (WebRTC, MSE, HLS, MJPEG over one WebSocket) | yes | - | no | M5 |
| `/api/stream.mp4`, `.m3u8`, `.mjpeg`, `.ts`, `.flv`, `.aac`, `.y4m`, `/api/frame.mp4` | yes | - | no | M5, M6 |
| `/api/onvif`, `/api/ffmpeg/devices`, `/api/ffmpeg/hardware`, `/api/hass`, `/api/homekit` | yes | - | no | M6, M8 |
| A control API of its own: configuration, paths, sessions listed and kicked, per protocol | - | yes (v3) | partial: `/api/streams` shows sources and consumers | M7 (native API v1, R7) |
| OpenAPI description | yes | yes | no | M5 |
| Web interface: streams, links, players, add a stream, configuration editor, log, connection graph | yes | no (a read page per path) | no | M5 |
| A JavaScript player that picks WebRTC, MSE, HLS or MJPEG by what the browser can do | yes (`video-rtc.js`) | per-protocol pages | no | M5 |
| Home Assistant core `go2rtc` integration (a go2rtc server HA talks to) | native | no | no: needs `/api/ws`, `/api/schemes` and a version HA accepts | M5 |
| Home Assistant add-on | yes | community | no | M9 |
| WebRTC Camera custom component, Frigate, Advanced Camera Card (clients of the go2rtc API) | yes | no | untested | M5 (tested against each) |
| openIVR | replaced by oxrtc since v2026.11.17.1 | no | done | - |

## Operations

| Feature | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| YAML configuration, go2rtc's keys | yes | its own | partial: the subset of [COMPATIBILITY.md](COMPATIBILITY.md) | M5 to M7, key by key |
| `${ENV}` substitution with defaults, systemd credentials | yes | env overrides (`MTX_...`) | no | M7 |
| Several `-c` files and inline overrides | yes | no | no | M7 |
| JSON schema for editors | yes | no | no | M7 |
| Reload without dropping readers | through the API | yes (file watched) | through the API (`PUT /api/streams`) | M7 (file watched) |
| Log: levels per module, JSON | yes | levels, destinations (stdout, file, syslog) | one level, go2rtc's format | M7 |
| Authentication: users, per-action and per-path permissions, by address | one user for the API, one for RTSP | yes (internal, hashed passwords, HTTP, JWT) | one user or token for the API, one for RTSP; **never open by default, loopback included** | M7 (R7) |
| Recording to disk (fMP4, MPEG-TS), retention | no (`stream.mp4?duration=`) | yes | no | M7 |
| Playback of recordings (list, get) | no | yes | no | M7 |
| Always-available streams (a placeholder while the source is away) | no | yes | no | M7 |
| Hooks (commands on connect, ready, read, record) | `exec`, `echo` | yes | never by default; opt-in commands with the `exec` allow-list, and **web hooks** (an HTTP call, no command) by default | M7 |
| Metrics (Prometheus) | no (JSON stats, connection graph) | yes | no (`/api/streams` counters) | M7 |
| Profiling endpoint | `/api/stack` | pprof | no | M7 (off by default) |
| Absolute timestamps routed (NTP, `PROGRAM-DATE-TIME`) | no | yes | partial: one wall-clock anchored timeline per track (D12), not exported | M5 (HLS), M6 (RTSP sender reports) |
| TLS on every listener (HTTPS, RTSPS, RTMPS), Unix socket, a base path, CORS | HTTPS, Unix socket, base path, CORS | yes | no | M7 |
| Preload (keep a stream open) | yes | `sourceOnDemand: no` | done | - |
| Stats per connection | yes | yes | done (JSON) | - |

## Distribution

| Item | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| One static binary | yes | yes | done (`oxrtc`, plus `oxrtc-exec` on Linux) | - |
| Targets | Windows (amd64, 386, arm64), Linux (amd64, 386, arm64, arm, armv6, mipsel), macOS (amd64, arm64), FreeBSD | Linux (amd64, armv6, armv7, arm64), Windows, macOS | windows-amd64, linux-amd64, linux-arm64 | M9 |
| Docker images, several architectures, FFmpeg inside | yes | yes (with and without FFmpeg, Raspberry Pi) | a Dockerfile in openIVR only | M9 |
| Home Assistant add-on repository | yes | no | no | M9 |
| Run at boot: systemd unit, Windows service | documented | documented | no | M9 |
| Signed releases, SBOM, third-party notices | no | no | SBOM and notices done; signing no | M9 |
| Distribution packages (Alpine, AUR, Nix, Homebrew, Scoop...) | community | community, OpenWrt | no | after 1.0 |

## Security posture (kept, whatever is added)

| | go2rtc | MediaMTX | oxrtc |
|---|---|---|---|
| Administration API with no credential | open on the LAN by default; loopback and Unix-socket callers always trusted | open to `any` user by default (publish, read); the API limited to loopback | refused: every call authenticated, loopback included (SEC-001) |
| Running commands | `exec:`, `echo:` sources, reachable through the API | hooks in the configuration file | no path by default (SEC-007); `exec` and hooks exist only behind `exec.allow_paths`, and never from the API unless the file allows it |
| The transcoder | an FFmpeg child | an FFmpeg child (hooks) | an FFmpeg child confined on Linux (no new privileges, limits, Landlock), killed with the server |
| Memory | unbounded queues in places | bounded write queue per reader | every buffer bounded by bytes and time (CORE-002) |
| Parsers | - | - | fuzz targets with proptest twins in `cargo test` (SEC-010) |
| Supply chain | - | - | `cargo deny`, `cargo audit`, SBOM per release |

Everything this file plans is added **inside** that posture: a new listener
has per-address limits and timeouts, a new parser has a fuzz target, a new
feature that can run a program or reach another host is off until the
configuration file names what it may do.

## The bench (the other half of the goal)

Scoreboard of the session of 2026-11-17 (openIVR `results_bench/`, 160 rows,
go2rtc 1.9.15, oxrtc 0.1.0, MediaMTX 1.21.3): **28 rows won, 104 tied (6 of
them with oxrtc behind the two others), 15 lost**. The lost rows and the
ties-behind are M4's work list, each with its hypothesis:

| Rows | What loses | First hypothesis (to measure before any change) |
|---|---|---|
| R01, R02 | recovery after a 10 s outage 7.1 s (go2rtc 5.4 s); after 20 s 4.3 s (MediaMTX 2.3 s) | the camera's silence is noticed only at the stall timeout (15 s), so it is back before oxrtc knows it left; and the retry cadence goes up to 5 s |
| B04, B05, B06 | more frames dropped and more freezes in the browser with 16 cameras or 50 viewers | a keyframe request replays the whole cached group (R24) to a viewer that is already behind; no pacing; timestamps taken at arrival |
| RC1, RC4 | FFmpeg children: more memory than go2rtc's (616 against 476 MiB), more CPU than MediaMTX's (218 against 127 % of a core) | MediaMTX's FFmpeg processes do less in the bench (25 frames a second where the source sends 30): equal work first; then one FFmpeg per camera for the sound, which a native transcode removes |
| RC2, RC3 | the tree's CPU and memory with nothing held; memory between sequential viewers | the idle grace keeps a transcoder 5 s after its last reader |
| RC1, RC2, RC3 | first frame of the MJPEG webcams through the H.264 transcode (warm 2.3 s against 0.7 to 1.0 s) | FFmpeg's start on a JPEG input (probing), and a wait still to find for the warm case |

The rule from here on is in [BENCHMARKS.md](BENCHMARKS.md): a release claims a
domain only when no row of it is lost, and every new protocol of M5 to M10
enters the bench with its own scenarios against whichever reference speaks it.
