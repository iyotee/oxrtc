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
| FFmpeg: any input it reads (files, HLS, devices), transcoding, hardware encoders | yes | through hooks | partial: transcodes of a stream or an RTSP URL to H.264, Opus and Motion JPEG (`#video=mjpeg` alone, on FFmpeg's standard output, D46), templates, `#input=`; no file, device, hardware, `#raw` | M6 (R12) |
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
| WebRTC over the go2rtc WebSocket API (`/api/ws`) | yes | no | done (F2, D39: both of go2rtc's dialects, candidates both ways, the session outliving its socket; go2rtc's player and Home Assistant's client library checked) | - |
| One MediaStream for the picture and the sound of a session | yes | yes | done (W1, D30: one stream id per session, a track per media line) | - |
| RTSP (TCP interleaved), codec filters in the query | yes | yes | done | - |
| RTSP over UDP, multicast, RTSPS, several audio tracks, `default_query` | `default_query`, several tracks | UDP, multicast, RTSPS | no | M6 |
| MSE: fragmented MP4 over WebSocket | yes | no | done (F3, D40: the browser's codec list picks the sources as a WebRTC offer does, the camera's own H.265 included; one fragment per frame from the cached keyframe; go2rtc's player in headless Edge) | - |
| MP4 over HTTP (progressive), one-frame MP4 (`frame.mp4`), timed clips (`duration=`) | yes | playback server only | done (F4, D42: go2rtc's `mp4`, `mp4=flac`, `mp4=all`, filters, `duration` on the media, `filename`; from the cached keyframe; FFmpeg and headless Edge's `<video src>` play it); not `rotate`, `scale`, nor Safari's redirect to HLS (F6) | - |
| HLS: MPEG-TS and fMP4 | yes (non-standard, by its own word) | yes | no | M5 |
| Low-Latency HLS | no | yes | no | M5 |
| MPEG-DASH | no | no | no | M5 (same segments as HLS) |
| MJPEG (multipart), JPEG snapshot (`width`, `height`, `rotate`, `cache`) | yes | no (FFmpeg hook) | done (F5, D46): `stream.mjpeg`, the `mjpeg` socket message, `frame.jpeg` with all four; a stream with no JPEG source encoded by one shared FFmpeg, where go2rtc serves it nothing; `hardware` not yet (R12) | - |
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
| AAC (MPEG-4 audio) | yes | yes | partial: carried over RTSP, and in MP4 over MSE (F3) and HTTP (F4) | M5 (HLS), M6 (RTMP) |
| G.722, G.726, MP3, AC-3, LPCM, FLAC | MP3, PCM | yes | L16 carried; a G.722 encoder in `oxrtc-audio` (bit for bit FFmpeg's), no G.722 track yet | M8 |
| PCM family packed as FLAC for MP4/MSE/HLS | yes | FLAC in MoQ, RTMP, HLS | partial: MSE and MP4 over HTTP (F1, F3, F4) | M5 (HLS) |
| G.711 resampled to 8 kHz for WebRTC, PCM to PCMA | yes, in process | no | done (W5, D37): G.711 and L16 to Opus, G.711 or L16 in process, no FFmpeg; AAC and other inputs still through FFmpeg | - |
| KLV and other data tracks | no | yes | no | M8 |

## API and integrations

| Surface | go2rtc | MediaMTX | oxrtc today | Closed by |
|---|---|---|---|---|
| `GET /api`, `/api/streams` (GET, PUT, DELETE), `/api/preload`, `/api/frame.jpeg`, `POST /api/webrtc` | yes | - | done | - |
| `/api/streams` `PATCH` and `POST` (`src`/`dst`), `/api/streams.dot`, `/api/schemes` | yes | - | yes, `POST` answering `501` until F10 and F12 (F7, D48) | M5 |
| `/api/config` (GET, POST, PATCH), `/api/restart`, `/api/exit`, `/api/log` | yes | - | yes, each off until the file allows it (`api.allow_*`); restart in process, on Windows too; exit answered first (F7, D48) | M5 |
| `/api/ws` (WebRTC, MSE, HLS, MJPEG over one WebSocket) | yes | - | partial: WebRTC (F2, D39), MSE (F3, D40), MJPEG (F5, D46); `hls`, `mp4` answered with go2rtc's error form | M5 (F4, F6) |
| `/api/stream.mp4`, `.m3u8`, `.mjpeg`, `.ts`, `.flv`, `.aac`, `.y4m`, `/api/frame.mp4` | yes | - | partial: `stream.mp4`, `frame.mp4` (F4, D42), `stream.mjpeg` (F5, D46) | M5, M6 |
| `/api/onvif`, `/api/ffmpeg/devices`, `/api/ffmpeg/hardware`, `/api/hass`, `/api/homekit` | yes | - | no | M6, M8 |
| A control API of its own: configuration, paths, sessions listed and kicked, per protocol | - | yes (v3) | partial: `/api/streams` shows sources and consumers | M7 (native API v1, R7) |
| OpenAPI description | yes | yes | yes, oxrtc's own at `GET /api/openapi.yaml`, held to the router by a test (F7) | M5 |
| Web interface: streams, links, players, add a stream, configuration editor, log, connection graph | yes | no (a read page per path) | no page of its own yet (F8); go2rtc's own pages run against oxrtc's API (F7: the stream list, the log, the connection graph, the configuration editor, the links, the player) | M5 |
| A JavaScript player that picks WebRTC, MSE, HLS or MJPEG by what the browser can do | yes (`video-rtc.js`) | per-protocol pages | no | M5 |
| Home Assistant core `go2rtc` integration (a go2rtc server HA talks to) | native | no | yes, through its client library `go2rtc-client` 0.4.0 running the integration's calls (F7, D48): the version check, `/api/schemes`, the camera set as `[source, ffmpeg:<camera>#audio=opus]`, the offer over `/api/ws` playing (146 video frames, 256 audio packets in 5 s), the second offer finding the stream set, preload, snapshot, the WHEP offer in JSON. Home Assistant itself not run (`pip install homeassistant` failed on this PC: `lru-dict` needs a C compiler) | M5 (F7) |
| Home Assistant add-on | yes | community | no | M9 |
| WebRTC Camera custom component, Frigate, Advanced Camera Card (clients of the go2rtc API) | yes | no | go2rtc's player, which the WebRTC Camera card embeds, plays from oxrtc (F2, F3, F5, F6, F7); Frigate and the Advanced Camera Card untested | M5 (tested against each) |
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

Scoreboard of the full bench of 2026-11-23 (round 1; openIVR `results_bench/`,
go2rtc 1.9.15, oxrtc 0.2.0 at `fc106eb`, MediaMTX 1.21.3, six repetitions A B B A,
oxrtc in its default mode): the D29 count **352 rows, 106 won, 185 tied, 4 tied
behind, 16 lost, 41 without verdict**; the stress rows apart (B05, B06; D35) 26: 14
won, 4 tied, 2 tied behind, 2 lost, 4 without verdict. The previous session
(2026-11-17, 3 repetitions) read 142 counted rows: 20, 92, 5, 14 and 11. The rows
still lost or tied behind, go2rtc / oxrtc / MediaMTX (mean +/- the 95 % half-width):

| Scenario | Row | go2rtc / oxrtc / MediaMTX | Class |
|---|---|---|---|
| RC1 | camera_5: its FFmpeg children's CPU (% of one core); per picture (ms) | 10.0 / 11.8 / 13.7; 3.35 / 3.95 / 4.61 | lost (go2rtc), 2 rows |
| RC4 | camera_5: its FFmpeg children's CPU; per picture | 7.7 / 12.3 / 10.5; 3.86 / 4.53 / 4.88 | lost (go2rtc), 2 rows |
| RC4 | camera_11: its FFmpeg children's CPU | 7.8 / 13.5 / 12.4 | lost (go2rtc) |
| RC4 | its FFmpeg children's CPU (all) | 106.1 / 122.1 / 128.6 | lost (go2rtc); over equal running time 125.4 / 127.0 / 139.5, no difference |
| RC4 | camera_12: its FFmpeg children's CPU | 3.3 / 5.2 / 3.8 | tied behind |
| RC5 | camera_1: longest gap between frames (ms) | 744 / 1095 / 2261 | lost (go2rtc) |
| RC5 | frames dropped by the browser per viewer | 1.6 / 13.9 / 13.6 | lost (go2rtc) |
| RC5 | freezes (browser) per viewer | 3.6 / 10.0 / 6.1 | tied behind |
| B03 | its FFmpeg children's RSS while every transcode runs (MiB) | 1634 / 1805 / 3431 | lost (go2rtc); two causes measured 2026-11-24: go2rtc's FFmpegs wait, small, for a keyframe for 20 s, and every oxrtc FFmpeg holds 14 MiB more (187 against 173.7) from the first time oxrtc cuts its 2 s input queue on this saturated machine; the tree's private bytes are oxrtc's lower (2980 against 3040-3150) |
| B07 | frames dropped (browser); tree CPU (%); tree RSS mean and max (MiB); tree CPU per delivered frame (ms); FFmpeg RSS while every transcode runs (MiB) | 0.0 / 18.4; 726 / 757; 1314 / 1566 and 1383 / 1598; 17.3 / 17.8; 1253 / 1498 (MediaMTX not run) | lost (go2rtc), 6 rows |
| L03 | first frame, cold (ms) | 2455 / 2705 / 2746 | lost (go2rtc); **no difference (oxrtc, go2rtc) since 2026-11-24** (M4 fix round 1, D25 amended): 2429 / 2405 / 2666, 3 reps A B B A |
| L01 | sound concealed (browser, % of the samples) | 0.00 / 0.02 / 0.00 | tied behind; a three-way tie in the M4 fix round 1 review's run (`m4f1r-L01`, 3 reps A B B A: 0.01 / 0.00 / 0.00), where the latency mean is won again (79.8 +/- 12.0 against MediaMTX 118.6 +/- 23.3) |
| B04 | frames dropped (browser) | 874.5 / 920.7 / 868.6 | tied behind |
| B05 (stress) | frames presented (fps); frames dropped | 4.84 / 4.39 / 7.84; 1308 / 1731 / 499 | lost (MediaMTX), outside the count |
| B05 (stress) | freezes (browser) | 65.6 / 69.3 / 53.8 | tied behind, outside the count |
| B06 (stress) | frames dropped (browser) | 966.4 / 967.5 / 936.1 | tied behind, outside the count |

M4 fix round 1 (2026-11-24, the report in openIVR's `docs/plans/handoff-2026-11-15/oxrtc-programme/M4-fix-1.md`):
**L03** was one offer of twenty (offer 5: go2rtc 115 ms, oxrtc 4910), its answer held a
second by the stale-session wait, which missed the keyframe go2rtc caught; fixed (D25
amended) and tied, oxrtc lowest. **B03's memory row** has two causes (the M4 fix round 1 review, full-length
runs with a per-process sampler): go2rtc's FFmpegs wait about 20 s, small, for the
camera's next keyframe, which oxrtc's decode at once from the cached group (D11); and on
this machine, 100 % busy under all three servers, no H.265 transcode keeps up, oxrtc cuts
each FFmpeg's input queue once it is 2 s old (59 cuts in 120 s) and resumes it at a
keyframe, and from its first cut each FFmpeg holds 14 MiB more for good (187 MiB against
go2rtc's 173.7; ten FFmpegs of ten, each within one sample of its first cut). Over the
tree, oxrtc's private bytes are the lower (2980 against 3040-3150 MiB, round 1), so the
row is lost on FFmpeg's working set only, at a load at which no transcode keeps up.
**B07's memory rows**: equal at equal work on a calm machine (157-158 MiB a video
transcode for both, after go2rtc's 20 s ramp), round 1's lower go2rtc state not
reproduced. **B07's frames dropped**: they follow the browser's keyframe requests (0, 1 and
2-7 PLIs: 0-3, 13-18 and 24-36 dropped, the six runs of round 1), which come within 0.2 s of a connection that was sent
the cached group in one burst; the price of a first picture 4 s sooner, a decision left
open. **The webcams' CPU rows**: x264 told the camera's rate (D36) codes 2 to 10 times
go2rtc's bits; a lead by code alone, measured offline, is x264's sliced threads (a third
of a 704x480 encode's CPU), not taken yet.

What these rows share: the CPU and memory rows are FFmpeg's (oxrtc's server
process is the cheapest of the three wherever the tables give it apart: RC1, RC4,
RC5, and per delivered frame in B02, B03 and B07), and oxrtc's webcam encodes send
its viewer 2 to 10 times go2rtc's bitrate (RC4 camera_5 1704 against
326 kbit/s); the RC5 rows are the XM cameras' keyframes arriving 0.4 to 0.9 s late,
which go2rtc's deeper browser buffer bridges (in live mode against go2rtc's live
session both read no difference). L01's latency, won on both rows with lip sync on
(79.3 against 115.5 ms, P95 105.8 against 136.6), closed W1b and W1c.

The session of 2026-11-17 (160 rows, go2rtc 1.9.15, oxrtc 0.1.0, MediaMTX 1.21.3)
read, as `bench/scoreboard.py` reads it since W6: **28 rows won, 98 tied, 6 tied
behind, 15 lost, 13 without verdict**; the D29 count alone (B05 and B06 left out
as stress rows, D35) 20 won, 92 tied, 5 tied behind, 14 lost, 11 without verdict.
Its lost rows and ties-behind were M4's work list, each with its hypothesis:

| Rows | What loses | First hypothesis (to measure before any change) |
|---|---|---|
| R01, R02 | recovery after a 10 s outage 7.1 s (go2rtc 5.4 s); after 20 s 4.3 s (MediaMTX 2.3 s) | **won since W2 (2026-11-18, D34)**: confirmed - the silence was noticed at the stall timeout (15 s) and the retries spaced up to 5 s; now 2.1 s, 1.2 s and 1.1 s after 10, 60 and 20 s (with the 10 s floor of the review; 1.1, 0.8 and 1.2 s with the first 5 s floor), against 5.5 s for go2rtc and 14.4, 8.7 and 4.2 s for MediaMTX (12.5 and 2.5 s for MediaMTX when its lone first new picture counts: the bench's rule waits for the next code to confirm it). The recovery is now counted to the first NEW picture (the old rule counted a frozen keyframe replayed on a keyframe request) |
| L01 | glass-to-glass latency with lip sync on: won in this session (a stream per track, the picture never held), a tie with MediaMTX since W1 (2026-11-17), when one stream per session made the browser hold the picture for the sound as it does for both references | **the mean won in one acceptance session of two since W1c (2026-11-20, D30 decision 6)**: the sound dated 20 ms after its picture (`webrtc.sound_lag_ms`, inside the owner's 45 ms): 70.0 +/- 11.5 ms against 104.8 +/- 17.7, then 95.4 +/- 47.2 against 110.2 +/- 24.1; the P95 a tie in both (96.7 against 133.6, 115.0 against 133.0), oxrtc lower in 5 runs of 6. What is left is the browser's own sound buffer, which wanders 30 ms between runs for either server; lip sync off (`webrtc.lip_sync: false`) wins both rows (8 ms) |
| B04, B05, B06 | more frames dropped and more freezes in the browser with 16 cameras or 50 viewers | **B04 tied since W3 (2026-11-18, D35); B06 tied on frames dropped and freezes, and its frames presented tied in the builder's session (11.61 against 12.38 and 11.36) but LOST to go2rtc in the review's (2026-11-19, 3 reps abba: 12.20 +/- 0.37 against 13.76 +/- 0.68; MediaMTX 12.56) - the browser decodes as many of oxrtc's frames (14.3 a second per viewer against 14.4-14.8) and shows 15 % fewer of them, every viewer getting each frame in the same instant where go2rtc's per-viewer lip-sync hold (6 to 200 ms) spreads them**: confirmed - the replay of R24 to a viewer already behind (its viewers received 1.3 to 1.8 times the camera's frames); pacing and timestamps were not the cause (nothing lost on loopback, RTP times are the source's steps). A request is now left to the camera's keyframe when it is near. **B05 still lost to MediaMTX** (4.98 frames presented a second against 9.55): its uneven sending makes the browser hold each picture 628 ms, which lets the decoder keep up; a playout-delay floor for the viewers that keep asking won B05's frames dropped against go2rtc but not MediaMTX, and lost B04's freezes, so it was withdrawn (D35). **Since 2026-11-19 B05 and B06 are stress rows** (the owner: two to five viewers at most, each in a browser of its own; D29 as amended, D35): still run and never made worse, outside the count of rows to win; the real case, five browsers on openIVR's live grid, is W6's scenario |
| RC1, RC4 | FFmpeg children: more memory than go2rtc's (616 against 476 MiB), more CPU than MediaMTX's (218 against 127 % of a core) | **RC1 no longer lost to go2rtc since W4 (2026-11-20, D36; 3 reps abba on the reviewed binary, a calm machine)**: no console host beside each FFmpeg on Windows (8 x 7 MiB) and no 20-picture rate guess (28 MiB a webcam transcode): the children's memory 546 +/- 9 MiB against go2rtc's 536 +/- 4, **tied behind by 10 MiB** (W5), the tree's 605 against 621 won; **since W5 (2026-11-21, D37; 6 reps abba, the owner's NVR running, camera_4 and camera_12 disabled by him, so four cameras): the children's memory WON (378.2 +/- 7.9 MiB against 420.0 +/- 8.2; the two sound FFmpegs gone), the children's CPU tied (236.9 +/- 9.5 against 233.0 +/- 6.4: the sound FFmpegs were 1 to 2 % of a core each, the rest is the x264 transcodes, W6)**; the FFmpeg CPU of the three Motion JPEG cameras **tied behind** go2rtc's by about 5 % at equal encoding (go2rtc re-encodes each JPEG in its own process, 104 % of a core against 15: the tree is 275 against 348). **RC4: four run-mean rows LOST to go2rtc** (the two fast webcams' FFmpeg CPU 56.3 and 54.9 % against 42.9 and 39.0, the children's CPU 219.7 against 173.6 and memory 554 against 423 MiB): its transcodes show their first picture 39 to 46 s into the 120 s run (oxrtc's 6 to 9 s) and are charged for less running time; over the second minute the children cost 221 % against 235 and 541 MiB against 524 - open, W6 (equal running time). MediaMTX's lower children CPU in RC1 (141 against 258) is confirmed as less work (25 of 30 pictures to the recorder): W6, then W5 for the sound's FFmpeg. **Since W6 (2026-11-21) the contenders do the same work**: MediaMTX's webcam path dated its pictures 1/25 s apart (its recorder read 25 of camera_5's 30, and nothing of camera_4's one a second); they are now dated as they arrive, and a webcam's recorder stream is relayed with no FFmpeg; one RC1 repetition reads the camera's rate on every reader of all three (29.97-29.98 for camera_5) and the same tracks. The encode level is each contender's (oxrtc tells x264 the camera's rate on every start since W6, D36; go2rtc and MediaMTX let FFmpeg guess), so the tables give each viewer's bitrate beside the CPU. RC4's run-mean rows read over equal work (`bench/work.py`, W4's acceptance session): the webcams' FFmpeg CPU per picture no difference (30.9 and 24.3 ms of a core against 25.2 and 24.0), the children's CPU while every transcode runs no difference (232 against 233 %), their memory then lost (588 against 543 MiB, a build with the sound FFmpegs W5 removed) |
| RC5 (the real case, since W6, 2026-11-21) | frames dropped by the browser, 16.1 +/- 3.9 per viewer with lip sync on (go2rtc 0.06, MediaMTX 20.0) and 13.4 in live mode; the browser's freezes in live mode, 20.1 +/- 0.8 against go2rtc's 10.0 +/- 3.7 (6 repetitions A B B A, camera_1, camera_2, camera_12, 15 viewers in five Edge processes) | **measured, not yet fixed (W8)**: each XM keyframe, every 6.37 s, reaches go2rtc and oxrtc alike 0.4 to 0.9 s late (the camera's stall); the browser holds 0.2 s of oxrtc's video (about none in live mode, `playout-delay` 0) against 0.3 to 0.6 s of go2rtc's, so the stall is a freeze in oxrtc's tiles. Won meanwhile: first frame 835 ms against 5301 and 5219, the server's CPU and memory |
| RC2, RC3 | the tree's CPU and memory with nothing held; memory between sequential viewers | **won since W4 (2026-11-20, D36)**: confirmed - a transcode now stops with its last consumer and a camera keeps its 5 s (`api.transcode_idle_grace_ms`, `api.idle_grace_ms`): RC3's tree 4.5 +/- 1.8 % of a core (a tie with go2rtc's 6.5 +/- 0.8, oxrtc lowest; MediaMTX 42.3) and 30.8 MiB against 66.3 and 243; RC2's 28.8 % and 87 MiB against 62.6 and 139 (MediaMTX 90 and 553) |
| RC1, RC2, RC3 | first frame of the MJPEG webcams through the H.264 transcode (warm 2.3 s against 0.7 to 1.0 s) | **won since W4 (2026-11-20, D36)**: both confirmed - FFmpeg read 20 pictures to guess the frame rate before it opened its input (the relay now announces the rate and its own transcodes skip the guess, with FFmpeg 8 or later), and the warm viewer waited for the webcam's declined sound transcode to sleep out its backoff (such a source is now passed over): warm 283 +/- 38 and 283 +/- 61 ms against go2rtc's 870 and 999 and MediaMTX's 631 and 771, cold 662 and 677 against 1230 and 1539, 1154 and 1354, RC1 392 and 444 against 867 and 986; the one-a-second webcam 0.35 to 1.7 s where all three took 20 s. With FFmpeg 6.1 or 7.1 the guess and its wait stay |

The rule from here on is in [BENCHMARKS.md](BENCHMARKS.md): a release claims a
domain only when no row of it is lost, and every new protocol of M5 to M10
enters the bench with its own scenarios against whichever reference speaks it.
