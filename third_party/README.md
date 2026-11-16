# third_party - patched dependencies

Each directory is a crates.io release copied verbatim, then patched, and wired
in through `[patch.crates-io]` in the workspace `Cargo.toml`. It is not a
workspace member (`exclude`), so `cargo fmt`, `cargo clippy` and the
workspace's lints do not apply to it: the diff below is the whole change.
Decision D10 in [docs/DECISIONS.md](../docs/DECISIONS.md) says why a copy and
when it goes (D20 for retina).

## h264-reader 0.9.0

retina 0.4.22 parses every H.264 SPS with it: from the SDP's
`sprop-parameter-sets` and from every SPS in band, on the camera side
(`crates/rtsp-client`) and on the publishing side (`crates/rtsp-server`), both
through retina's depacketizer in `crates/depacketize` since D26.

0.9.0 refuses an SPS whose picture exceeds the MaxFS of the level it declares
(H.264 Table A-1), and checks `max_dec_frame_buffering` against that level's
MaxDpbMbs. Encoders and cameras declare levels too low: go2rtc's H.264 template
(`-level:v 4.1`) encoding 2304x1296 gives 11664 macroblocks against level 4.1's
8192, and x264 encodes it without complaint. retina then refused every
keyframe, and no frame came out of the stream at all (the bench's B03, no
picture in the browser; go2rtc plays the same stream). Decoders, FFmpeg's
included, ignore the declared level.

The patch, in `src/nal/sps.rs`:

1. the frame-size bound is level 6.2's MaxFS (139264 macroblocks, the bound
   0.9.0 already used for an unknown level) whatever the declared level - the
   size of a picture stays bounded;
2. `max_dec_frame_buffering` is checked against level 6.2's MaxDpbMbs when the
   picture exceeds its declared level's MaxFS, against the declared level's
   otherwise.

Both changes are marked `oxrtc patch` in the source. Reproduced by
`an_sps_over_its_declared_level_is_read` in
`crates/depacketize/src/tests.rs`, which fails against the unpatched
crate.

To check the copy against the release (the registry cache holds it once the
release has been built once):

```sh
diff -r ~/.cargo/registry/src/*/h264-reader-0.9.0 third_party/h264-reader
```

Only `src/nal/sps.rs` differs (and the release's `.cargo_vcs_info.json` and
`.cargo-ok`, which are not copied).

## retina 0.4.22

retina's RTSP connection is a `TcpStream` it opens itself, so a camera that
speaks RTSP over TLS (`rtsps://`, which openIVR's TLS policy promotes cameras
to) could not be read (D20, R8). The patch makes the stream the caller's, in
`src/tokio.rs` and `src/client/mod.rs`, every change marked `oxrtc patch`:

1. `tokio.rs`: `Transport` gains a `Boxed(Box<dyn RtspStream>)` variant (read
   into the first nonempty slice, like the in-memory test stream; write, flush
   and shutdown forwarded) and `Connection::from_boxed`;
2. `client/mod.rs`: the public trait `RtspStream` (`AsyncRead + AsyncWrite +
   Send + Unpin`), the struct `ConnectedStream` (the stream and its two
   addresses), the trait `Connector` (opens the connection to a URL) and
   `SessionOptions::connector`;
3. `RtspConnection::connect` asks the connector, when there is one, instead of
   opening a TCP socket - for the session and for the fresh connections of a
   background TEARDOWN, which carry the same options - and `validate_url`
   accepts the scheme `rtsps` when a connector is set;
4. `client/mod.rs` and `client/parse.rs`: `Stream::format_specific_params()`,
   the SDP's `a=fmtp` value for the stream's payload type, which the parser
   already read to build retina's own depacketizer and then dropped. The
   camera ingest depacketizes the raw packets itself (D26, R11) and builds the
   same depacketizer from it (AAC's `config`, H.264's
   `sprop-parameter-sets`); a field and an accessor, the parser's logic is
   not touched.

Nothing else differs: no code of the TCP path, the parser's logic, the
depacketizers or the session logic is touched. `tests/rtsps.rs` in `crates/rtsp-client` runs
the whole session over the hook.

To check the copy against the release:

```sh
diff -r ~/.cargo/registry/src/*/retina-0.4.22 third_party/retina
```

Only `src/tokio.rs`, `src/client/mod.rs` and `src/client/parse.rs` differ (and the release's
`.cargo-ok` and `.cargo_vcs_info.json`, which are not copied). The copy goes
when retina has the hook, or TLS, and the accessor itself.
