# oxrtc-mp4

A fragmented MP4 writer for oxrtc (roadmap M5, F1; decision D38): the core's
tracks and frames in, an init segment and media fragments out. No I/O; MSE,
MP4 over HTTP, HLS, DASH and recording are written with it.

```rust
let mut writer = Writer::new(&tracks, Options { fragmenting: Fragmenting::EveryFrame, ..Options::default() })?;
send(writer.init_segment());                 // ftyp + moov
// MediaSource.isTypeSupported(writer.mime_type()), e.g. video/mp4; codecs="avc1.64001e,flac"
for (track, frame) in frames {
    if let Some(fragment) = writer.push(track, &frame)? {
        send(&fragment.bytes);               // moof + mdat
    }
}
```

`Fragmenting::GroupOfPictures` closes a fragment before each keyframe of the
first video track, `Fragmenting::Duration(d)` once it holds `d` (and before
each keyframe too); `flush()` closes one at any time. A `Fragment` says
whether it is independent and where it sits on the core's timeline (`span`).

## Proofs

- `cargo test -p oxrtc-mp4`: the writer read back by a reader written apart
  from it (`src/reader.rs`), the fuzz targets' harnesses under proptest, and
  `tests/ffmpeg.rs` - real FFmpeg encodes (libx264, libx265, AAC, libopus,
  A-law) written, read back by ffprobe (codecs, one packet per frame, the
  source's decode and presentation times, B-frames included) and decoded by
  ffmpeg with no error; A-law packed as FLAC decodes to exactly the samples of
  FFmpeg's own A-law decoder. Skipped when FFmpeg is absent
  (`OXRTC_FFMPEG`, `OXRTC_FFPROBE`; `OXRTC_NO_SKIP` makes a skip fail).
- `fuzz/`: `mp4_reader` and `mp4_writer` (`fuzz/README.md`).
- A real browser, by hand (not in `cargo test`): MSE in headless Edge through
  Playwright, for H.264 with AAC, with Opus and with A-law as FLAC, one
  fragment per frame appended at its own time, and a picture of uneven steps
  with 200 ms missing. It asserts that `MediaSource.isTypeSupported` takes the
  MIME type and each codec string, that the picture is 640x360, that playback
  advances over 4 s, that sound is decoded, and that one range is buffered.

  ```sh
  OXRTC_MP4_BROWSER_DIR=/tmp/mse cargo test -p oxrtc-mp4 --test ffmpeg -- --ignored browser_files
  node crates/mp4/tests/browser/mse.mjs /tmp/mse
  ```

  Playwright is taken from openIVR's frontend beside this clone
  (`../openIVR/frontend`), or from the `package.json` that
  `OXRTC_PLAYWRIGHT_FROM` names; `OXRTC_BROWSER_CHANNEL` picks the browser
  (`msedge` by default, `chrome`, `chromium`).

Run on 2026-11-21 with Edge 157.0.4341.60 on Windows 11: four cases of four played
(5.7 to 5.9 s of playback in as long, one buffered range, no stall, no
dropped frame but two in the uneven case).
