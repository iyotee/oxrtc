# Threat model

## Assets

1. Camera credentials (in the configuration and in source specs).
2. The video itself - live and through relays.
3. The host: a media server is one remote-code-execution away from a pivot
   into the LAN where the cameras live.

## Adversaries

| Who | Reaches | Wants |
|---|---|---|
| LAN host | WebRTC UDP/TCP, any port bound beyond loopback | to watch, to crash, to pivot |
| Local process / SSRF through the NVR backend | loopback API and RTSP | to reconfigure streams, read credentials, run commands |
| Compromised camera | the RTSP client, as a server | to exploit the parser, to exhaust memory |
| Remote viewer through a TURN relay | WebRTC | to watch more than allowed |

## Threats and their controls

| Threat | Control | Verified by |
|---|---|---|
| Command execution through a source | no command-running scheme exists; FFmpeg argv from operator templates only | `SEC-007`: grep + test that `exec:` is refused |
| Admin API reached from loopback or SSRF | authentication on every call; loopback is not trusted | `SEC-001` |
| Credentials in logs, errors, `/api/streams` | `SourceSpec` displays redacted; raw only via `expose()` | `SEC-005`: property test + log capture test |
| Malformed RTSP/RTP/SDP from a camera | retina's parsers (fuzzed upstream); our own parsers fuzzed; no panic on input | `SEC-010`, `CORE-001` |
| Malformed `ANNOUNCE` body or RTP from a local process (a publisher, before its token is checked) | the SDP parser (sdp-types 0.2: 0.1 panicked on a malformed line before a session line) and retina's depacketizers, both fuzzed through our harnesses. Around retina: the losses reported to it between two frames are kept under half its 16-bit counters (large sequence gaps at one timestamp overflowed them: a panic in a debug build, a wrong count in release), and the frame cap applies to the frame that comes out (AVCC makes tiny NAL units up to five times larger than the payloads counted on the way in) | `fuzz/` targets `sdp_announce`, `depacketize`; `a_malformed_line_before_a_session_line_is_refused`, `large_sequence_gaps_never_overflow_the_loss_count`, `a_frame_over_the_cap_never_comes_out` |
| Memory exhaustion by a camera or a viewer | every buffer bounded (`Limits`); frame size cap; GOP cache cap; backlog cap | `CORE-002`, `SEC-006` |
| Connection floods | a total and a per-address limit on the API, RTSP and WHEP (loopback counted like any address, a global IPv6 peer by its `/64`, a link-local one by its address: D9; refusals are logged at most once a second, with their count, so a flood does not flood the log); idle timeouts; body caps. Not yet: a deadline on a whole request (M3 "request timeouts") | `SEC-006` |
| Datagram floods on the WebRTC socket (one socket carries every session) | an oversized datagram is read whole (a 64 KiB read buffer: a smaller one made it a receive error on Windows) and a per-datagram error never pauses the loop; the kernel receive buffer is asked at 4 MiB - at the Windows default of 64 KiB a flood of 9000-byte datagrams made the kernel drop a real client's binding requests, and 3 connections in 10 failed. Volume itself is not bounded in-process: a flood faster than the machine reads still drops packets | `oversized_junk_datagrams_do_not_stall_the_socket` (flood from its own OS thread, connection under 3 s) |
| Slow senders (a request trickled byte by byte) | the RTSP framing examines each byte once and parses a request at most three times, so work grows with what is sent, not its square (R10); the request size cap still applies. The *time* is not bounded yet: the RTSP silence timeout restarts at every byte, so a trickle holds its connection (one of its address's share) until it reaches the size cap - a request deadline is M3's "request timeouts" | `work_stays_linear_on_a_slow_sender`, `a_request_sent_byte_by_byte_is_answered` |
| SSRF through a camera URL | a source can only reach the destinations its scheme implies; the egress policy (R2) can restrict address ranges per installation | `SEC-004` |
| Header/argument injection through a spec | control characters refused at parse; FFmpeg templates split once, the input substituted as one argument | `SEC-007` |
| DTLS/SRTP weaknesses | str0m with its rust-crypto backend (aws-lc only for the certificate: D3, R15); no custom crypto | dependency audit |
| Dependency compromise | `cargo deny` (`deny.toml`: permissive licences only, RustSec advisories and yanked crates denied, crates.io the only source), `cargo audit`, pinned toolchain, `Cargo.lock` committed, CI actions pinned by commit and tools by version, a CycloneDX SBOM per release binary | `ci.yml` job `supply-chain`, `release.yml` |

## What is deliberately trusted

- The configuration file: the operator writes it. It may name FFmpeg templates.
- FFmpeg's output: it is parsed by our own depacketizers like any camera
  stream, with the same limits.
