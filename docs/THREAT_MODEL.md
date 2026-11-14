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
| Memory exhaustion by a camera or a viewer | every buffer bounded (`Limits`); frame size cap; GOP cache cap; backlog cap | `CORE-002`, `SEC-006` |
| Connection floods | per-IP connection limits on API, RTSP and WHEP; request timeouts; body caps | `SEC-006` |
| SSRF through a camera URL | a source can only reach the destinations its scheme implies; the egress policy (R2) can restrict address ranges per installation | `SEC-004` |
| Header/argument injection through a spec | control characters refused at parse; FFmpeg templates split once, the input substituted as one argument | `SEC-007` |
| DTLS/SRTP weaknesses | str0m with a pure-Rust crypto backend; no custom crypto | dependency audit |
| Dependency compromise | `cargo deny` (licenses, advisories, sources), pinned toolchain, `Cargo.lock` committed | CI |

## What is deliberately trusted

- The configuration file: the operator writes it. It may name FFmpeg templates.
- FFmpeg's output: it is parsed by our own depacketizers like any camera
  stream, with the same limits.
