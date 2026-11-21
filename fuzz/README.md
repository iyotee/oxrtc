# Fuzzing

cargo-fuzz targets for the code that reads the network. The crate is its own
workspace (libFuzzer needs a nightly compiler and sanitizer flags the main
workspace is never built with); `rust-toolchain.toml` stays pinned, and a
dated nightly is named on the command line.

| Target | What it feeds | Harness |
|---|---|---|
| `rtsp_request` | bytes on an RTSP connection, cut into reads of a chosen size: requests, interleaved frames, keep-alives | `oxrtc_rtsp_server::fuzzing::rtsp_stream_bytes` |
| `sdp_announce` | an `ANNOUNCE` body, then each kept track into its depacketizer | `fuzzing::sdp_announce` |
| `sdp_describe` | any track list through `DESCRIBE`'s SDP and back through `ANNOUNCE`'s parser | `fuzzing::sdp_roundtrip` |
| `depacketize` | RTP packets from a publisher into the H.264, H.265, Opus, G.711, L16 and AAC depacketizers | `fuzzing::depacketize_bytes` |
| `packetize` | frames through this server's packetizers and back through the depacketizers | `fuzzing::packetize_bytes` |
| `jpeg_frame` | a JPEG file, or a scan behind valid headers (with the standard Huffman tables or tables of its own, coded again), through the RTP/JPEG reader, the fragments and the packetizer, and back through the depacketizer | `fuzzing::jpeg_frame` |
| `authorization` | the parameters of an `Authorization: Digest` header, the way a reader sends them: quoted and bare values, escapes, commas inside quotes; read, written out and read again | `fuzzing::authorization` |
| `wav_stream` | a WAV stream as an HTTP server sends it (openIVR's sound de-duplicator): the header with its chunks, then the samples, cut into reads of a chosen size; the header read within 4 KiB, less than one sample frame held after it, the same format and samples whatever the reads | `oxrtc_audio::fuzzing::wav_bytes` |
| `multipart` | a webcam's `multipart/x-mixed-replace` stream, cut at a chosen read size and in one piece | `oxrtc_http_client::fuzzing::multipart_bytes` |
| `whep_offer` | a WHEP offer, the SDP a remote browser writes | `oxrtc_webrtc::OfferedCodecs::from_sdp` |
| `turn_datagrams` | what a TURN server (or a spoofer of its address) sends the TURN client: datagrams, answers to its requests in flight, ChannelData, with data sent and time passing | `oxrtc_webrtc::fuzzing::turn_datagrams` |
| `turn_stream` | what a TURN server sends over TCP, where nothing marks a message's end but its own header: the byte stream cut into reads of any size, the same messages whatever the reads, each taken by the allocation, nothing buffered beyond one message, a longer one read past | `oxrtc_webrtc::fuzzing::turn_stream` |
| `ice_url` | a `webrtc.ice_servers` URL | `oxrtc_webrtc::fuzzing::ice_url` |
| `sdp_fragment` | the body of a `PATCH` on a WHEP session resource (`application/trickle-ice-sdpfrag`): its user name fragment and its candidate lines, at most 16 | `oxrtc_webrtc::fuzzing::sdp_fragment` |
| `answer_streams` | an SDP answer as str0m serialises it from a browser's offer, through the rewrite that names one stream for the session: its `a=msid`, `a=ssrc ... msid:` and `a=msid-semantic` lines, whatever an offer made str0m write after an id | `oxrtc_webrtc::fuzzing::answer_streams` |
| `mp4_reader` | any bytes into the fragmented MP4 reader the writer's tests use (`crates/mp4/src/reader.rs`), as an init segment and as a fragment: 64-bit sizes, `tfhd` and `trex` defaults, every base data offset rule, two samples sharing a byte refused; every sample's bytes within the input | `oxrtc_mp4::fuzzing::reader_bytes` |
| `mp4_writer` | tracks (H.264, H.265, AAC, Opus, G.711, L16; parameter sets and AAC configurations taken from the input), a fragmenting and frames of every shape through the fragmented MP4 writer, read back by that reader: decode times contiguous across fragments, each accepted frame's bytes, composition offset and flags as given, FLAC frames with their sync and CRC, the fragment's independence and span as its samples say | `oxrtc_mp4::fuzzing::writer_bytes` |
| `ws_message` | a text frame of `/api/ws` (go2rtc's WebSocket API): a frame taken as a message is JSON read whole, an offer in either dialect carries the value it was sent with, a candidate is one line without `a=`, an `mse` codec list makes a wish of known codecs of the right kinds, each once, and every message written from what was read is JSON that parses back to the same fields | `oxrtc_api::fuzzing::ws_message` |
| `source_spec` | a source spec and a consumer query (`?video=h264&audio`) | `oxrtc_core::SourceSpec::parse`, `Want::from_query` |

The harnesses live in the crates (`crates/rtsp-server/src/fuzzing.rs`,
`crates/http-client/src/fuzzing.rs`, `crates/webrtc/src/fuzzing.rs`,
`crates/audio/src/fuzzing.rs`, `crates/mp4/src/fuzzing.rs` and `crates/api/src/fuzzing.rs`, behind the `fuzzing` feature) and return an error naming the invariant that broke:
the read loop's buffer bound, no stall with a full buffer, the same messages
whatever the read size, our own response parsing back with its `CSeq`,
tracks read back as they were described, frames out as they went in. A panic
is a defect by itself.

## Running

```sh
rustup toolchain install nightly-2026-10-31 --profile minimal
cargo +nightly-2026-10-31 install --locked cargo-fuzz@0.13.2
cd fuzz
cargo +nightly-2026-10-31 fuzz run rtsp_request corpus/rtsp_request seeds/rtsp_request -- -max_total_time=3600
```

The first directory is the working corpus (ignored by git), the second the
committed seeds.

`Cargo.lock` is committed and holds every crate the workspace's lock holds at
the same version (`fuzz_lock_follows_the_workspace_lock` checks it), so the
fuzz job builds what `cargo deny` has seen. After a dependency change, copy
the root `Cargo.lock` here and run `cargo metadata --format-version 1 > /dev/null`
in this directory: it drops what the fuzz crate does not use and adds
`libfuzzer-sys`. A crash is written to `artifacts/<target>/`; once it is
fixed, copy the input into `seeds/<target>/` so it is replayed for good.

On Windows the build needs Visual Studio's AddressSanitizer component
(`clang_rt.asan_dynamic_runtime_thunk-x86_64.lib`); `--sanitizer none` does
not link there (the coverage counters come from the ASan runtime). The CI job
(`.github/workflows/fuzz.yml`, started by hand) runs every target for an hour
on Linux.

## On stable, in `cargo test`

The same harnesses run under proptest in `cargo test`
(`crates/rtsp-server/src/fuzzing_tests.rs`, `crates/http-client/src/fuzzing_tests.rs`,
`crates/audio/src/fuzzing_tests.rs`, `crates/mp4/src/fuzzing_tests.rs`, `crates/webrtc/tests/offer_props.rs`), with generators close enough to valid
input to get past the first check, and `fuzz_seeds_replay` passes every seed of
the six `oxrtc-rtsp-server` targets and of `multipart` through its harness
(`the_committed_seeds_pass` those of `mp4_reader` and `mp4_writer`, which
`write_seeds`, an ignored test of `oxrtc-mp4`, writes again). A longer run:

```sh
OXRTC_PROPTEST_CASES=200000 CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true   CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true cargo test --release -p oxrtc-rtsp-server --lib fuzzing
```

Keep the overflow checks on: a release build wraps integer overflow
silently, and the first defect these harnesses found that way (retina's
16-bit loss counters, see `crates/depacketize/src/lib.rs`) was invisible to a plain
`--release` soak of 100,000 cases while a debug run of 1,000 hit it. cargo-fuzz
builds with debug assertions, hence overflow checks, by default.
