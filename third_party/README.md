# third_party - patched dependencies

Each directory is a crates.io release copied verbatim, then patched, and wired
in through `[patch.crates-io]` in the workspace `Cargo.toml`. It is not a
workspace member (`exclude`), so `cargo fmt`, `cargo clippy` and the
workspace's lints do not apply to it: the diff below is the whole change.
Decision D10 in [docs/DECISIONS.md](../docs/DECISIONS.md) says why a copy and
when it goes.

## h264-reader 0.9.0

retina 0.4.22 parses every H.264 SPS with it: from the SDP's
`sprop-parameter-sets` and from every SPS in band, on the camera side
(`crates/rtsp-client`, through retina's client) and on the publishing side
(`crates/rtsp-server`, through retina's depacketizer).

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
`crates/rtsp-server/src/depacketize.rs`, which fails against the unpatched
crate.

To check the copy against the release (the registry cache holds it once the
release has been built once):

```sh
diff -r ~/.cargo/registry/src/*/h264-reader-0.9.0 third_party/h264-reader
```

Only `src/nal/sps.rs` differs (and the release's `.cargo_vcs_info.json` and
`.cargo-ok`, which are not copied).
