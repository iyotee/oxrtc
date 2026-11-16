//! The fuzz targets' harnesses on stable, under proptest: random bytes, and
//! generated inputs close enough to valid ones to reach past the first check.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more for a soak run with
/// `OXRTC_PROPTEST_CASES=200000`; a `--release` soak must keep overflow
/// checks on (`CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true`, see
/// `fuzz/README.md`), or it cannot see an arithmetic overflow.
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// An SDP `ffmpeg` announces for an H.264 + G.711 publication.
const FFMPEG_SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=No Name\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
a=tool:libavformat 62.3.100\r\nm=video 0 RTP/AVP 96\r\nb=AS:200\r\na=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z2QAHqwsaoLA9puCgIKgAAADACAAAAMD0IAA,aO4xshsA; profile-level-id=64001E\r\n\
a=control:streamid=0\r\nm=audio 0 RTP/AVP 8\r\na=control:streamid=1\r\n\
m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/16000/1\r\n\
a=fmtp:97 profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3; config=1408\r\n\
a=control:streamid=2\r\n";

fn method() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "OPTIONS",
        "DESCRIBE",
        "SETUP",
        "PLAY",
        "TEARDOWN",
        "GET_PARAMETER",
        "SET_PARAMETER",
        "ANNOUNCE",
        "RECORD",
        "PAUSE",
        "X-UNKNOWN",
    ])
}

fn uri() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["rtsp://127.0.0.1:8554", "rtsp://[::1]:1", "rtsp://host", "*"]),
        "[a-zA-Z0-9_./%-]{0,24}",
        prop::option::of("(video|audio)(=[a-z0-9,]{0,12})?(&(video|audio)(=[a-z0-9,]{0,12})?)?"),
    )
        .prop_map(|(base, path, query)| {
            if base == "*" {
                return base.to_owned();
            }
            let mut uri = format!("{base}/{path}");
            if let Some(query) = query {
                uri.push('?');
                uri.push_str(&query);
            }
            uri
        })
}

fn header() -> impl Strategy<Value = String> {
    prop_oneof![
        "[0-9]{1,12}".prop_map(|v| format!("CSeq: {v}")),
        "[ -~]{0,20}".prop_map(|v| format!("CSeq: {v}")),
        prop::sample::select(vec![
            "RTP/AVP/TCP;unicast;interleaved=0-1",
            "RTP/AVP/TCP;unicast;interleaved=4-7;mode=record",
            "RTP/AVP/TCP;interleaved=255",
            "RTP/AVP;unicast;client_port=5000-5001",
            "RTP/AVP/UDP;unicast,RTP/AVP/TCP;unicast;interleaved=2-3",
            "RTP/AVP/TCP;unicast;interleaved=9-",
            "RTP/AVP/TCP;mode=\"RECORD\"",
            "",
        ])
        .prop_map(|v| format!("Transport: {v}")),
        "[ -~]{0,40}".prop_map(|v| format!("Transport: {v}")),
        "[0-9A-F]{1,16}(;timeout=[0-9]{1,3})?".prop_map(|v| format!("Session: {v}")),
        "[A-Za-z-]{1,12}: [ -~]{0,30}",
    ]
}

/// One request (a body when `Content-Length` is given) or one interleaved
/// frame.
fn message() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => (method(), uri(), prop::collection::vec(header(), 0..6), prop::option::of("[ -~\r\n]{0,80}")).prop_map(
            |(method, uri, headers, body)| {
                let mut text = format!("{method} {uri} RTSP/1.0\r\n");
                for header in headers {
                    text.push_str(&header);
                    text.push_str("\r\n");
                }
                if let Some(body) = &body {
                    text.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
                text.push_str("\r\n");
                text.push_str(body.as_deref().unwrap_or_default());
                text.into_bytes()
            }
        ),
        1 => (any::<u8>(), prop::collection::vec(any::<u8>(), 0..300)).prop_map(|(channel, payload)| {
            let mut frame = vec![b'$', channel];
            frame.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
            frame.extend(payload);
            frame
        }),
        1 => Just(b"\r\n".to_vec()),
    ]
}

/// A valid RTP packet carrying `payload`.
fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, (u8::from(marker) << 7) | 96];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&[1, 2, 3, 4]);
    p.extend_from_slice(payload);
    p
}

/// Payloads shaped like H.264/H.265 single NALs, aggregates and fragments,
/// with random bodies.
fn nal_payload() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(vec![
            vec![0x65],
            vec![0x41],
            vec![0x67],
            vec![0x18],
            vec![0x7c, 0x85],
            vec![0x7c, 0x05],
            vec![0x7c, 0x45],
            vec![0x26, 0x01],
            vec![0x62, 0x01, 0x93],
            vec![0x62, 0x01, 0x13],
            vec![0x62, 0x01, 0x53],
            vec![0x60, 0x01],
            vec![],
        ]),
        prop::collection::vec(any::<u8>(), 0..1400),
    )
        .prop_map(|(head, body)| [head, body].concat())
}

#[test]
fn harnesses_accept_their_own_valid_inputs() {
    let request = b"DESCRIBE rtsp://127.0.0.1:8554/cam?video=h264&audio RTSP/1.0\r\nCSeq: 2\r\n\r\n";
    rtsp_stream(request, 4096, 7).unwrap();
    // It is read as the one request it is, not refused as malformed (both
    // pass `rtsp_stream`, which compares two cuts of the same bytes).
    let (seen, end) = transcript(request, 4096, 7).unwrap();
    assert!(matches!(seen.as_slice(), [one] if one.starts_with("Request")), "{seen:?} then {end:?}");
    assert_eq!(end, None);
    sdp_announce(FFMPEG_SDP.as_bytes()).unwrap();
    // A valid file goes the whole way (the reader takes it, the packets read
    // back); a scan behind its headers too.
    let seed = jpeg_harness_seed();
    assert!(crate::jpeg::parse(seed).is_ok());
    jpeg_frame(&[&[0u8][..], seed].concat()).unwrap();
    jpeg_frame(&[&[9u8][..], &[0x55; 3000]].concat()).unwrap();
    // A file coded with tables of its own is coded again, and goes the whole way too.
    let optimized = crate::fuzzing::jpeg_harness_seed_optimized();
    assert!(crate::jpeg::parse(optimized).is_ok());
    jpeg_frame(&[&[0u8][..], optimized].concat()).unwrap();
    jpeg_frame(&[&[2u8][..], &[0x55; 3000]].concat()).unwrap();
    assert!(crate::jpeg::parse(&[seed.split_at(seed.len() - 40).0, &[0x55; 3000], &[0xff, 0xd9]].concat()).is_ok());
    assert_eq!(parse_announce(FFMPEG_SDP.as_bytes(), MAX_TRACKS).unwrap().len(), 3);
    for codec in Codec::ALL {
        packetize_roundtrip(codec, &[vec![0x11; 3000], vec![0x22; 17]]).unwrap();
    }
    // The depacketize harness's cap is within reach and checked: 3000
    // one-byte NAL units (15 kB of AVCC) are refused, not let through.
    let tiny: Vec<Vec<u8>> =
        (0..3001u16).map(|seq| rtp(seq, if seq < 3000 { 0 } else { 3000 }, false, &[0x41])).collect();
    assert_eq!(depacketize(0, &tiny), Ok(()));
}

/// Every seed of `fuzz/seeds/<target>/` (and every crash a fuzzer found,
/// once it is copied there) through its harness, on stable, in `cargo test`.
#[test]
fn fuzz_seeds_replay() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    type Harness = fn(&[u8]) -> Result<(), String>;
    let harnesses: [(&str, Harness); 7] = [
        ("rtsp_request", rtsp_stream_bytes),
        ("sdp_announce", sdp_announce),
        ("sdp_describe", |data| sdp_roundtrip(&tracks_from_bytes(data))),
        ("depacketize", depacketize_bytes),
        ("packetize", packetize_bytes),
        ("jpeg_frame", jpeg_frame),
        ("authorization", authorization),
    ];
    let mut replayed = 0;
    for (target, harness) in harnesses {
        for entry in std::fs::read_dir(seeds.join(target)).unwrap() {
            let path = entry.unwrap().path();
            let data = std::fs::read(&path).unwrap();
            assert_eq!(harness(&data), Ok(()), "{}", path.display());
            replayed += 1;
        }
    }
    assert!(replayed >= 14, "only {replayed} seeds found under {}", seeds.display());
}

/// The seeds reach past the first check: the reader seed is seven messages,
/// the announce seed two tracks.
#[test]
fn fuzz_seeds_are_meaningful() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    let reader = std::fs::read(seeds.join("rtsp_request/reader")).unwrap();
    let mut framer = Framer::new(4096);
    framer.extend(&reader[3..]);
    let mut count = 0;
    while let Ok(Some(_)) = framer.next() {
        count += 1;
    }
    assert_eq!(count, 7);
    let sdp = std::fs::read(seeds.join("sdp_announce/ffmpeg")).unwrap();
    assert_eq!(parse_announce(&sdp, MAX_TRACKS).unwrap().len(), 2);
}

/// The fuzz crate builds what the server is built with: its `Cargo.lock` is
/// committed (not ignored), and every crate it shares with the workspace's
/// lock is at a version that lock holds, so the fuzz job never resolves
/// crates `cargo deny` has not seen. After a dependency change: copy the
/// root `Cargo.lock` into `fuzz/` and run `cargo metadata` there.
#[test]
fn fuzz_lock_follows_the_workspace_lock() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ignored = std::fs::read_to_string(root.join("fuzz/.gitignore")).unwrap();
    assert!(
        !ignored.lines().any(|line| line.trim().trim_start_matches('/') == "Cargo.lock"),
        "fuzz/Cargo.lock is ignored"
    );
    let packages = |path: &str| -> Vec<(String, String)> {
        let lock = std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        let lines: Vec<&str> = lock.lines().collect();
        lines
            .windows(2)
            .filter_map(|pair| {
                let name = pair[0].strip_prefix("name = \"")?.strip_suffix('"')?;
                let version = pair[1].strip_prefix("version = \"")?.strip_suffix('"')?;
                Some((name.to_owned(), version.to_owned()))
            })
            .collect()
    };
    let workspace = packages("Cargo.lock");
    let fuzz = packages("fuzz/Cargo.lock");
    assert!(fuzz.iter().any(|(name, _)| name == "retina"), "fuzz/Cargo.lock lists no retina");
    let drifted: Vec<&(String, String)> = fuzz
        .iter()
        .filter(|(name, _)| workspace.iter().any(|(n, _)| n == name))
        .filter(|package| !workspace.contains(package))
        .collect();
    assert!(drifted.is_empty(), "fuzz/Cargo.lock differs from Cargo.lock: {drifted:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random bytes, random read sizes, random request limits.
    #[test]
    fn rtsp_random_bytes(data in prop::collection::vec(any::<u8>(), 0..4000), max in 1usize..3000, chunk in 1usize..600) {
        prop_assert_eq!(rtsp_stream(&data, max, chunk), Ok(()));
    }

    /// Streams of plausible requests and frames, cut at random read sizes:
    /// same messages whatever the cut, and every request through the
    /// dispatcher's header readers and the `CSeq` echo.
    #[test]
    fn rtsp_plausible_streams(messages in prop::collection::vec(message(), 1..12), max in 64usize..4096, chunk in 1usize..200) {
        let data = messages.concat();
        prop_assert_eq!(rtsp_stream(&data, max, chunk), Ok(()));
    }

    /// The byte-driven entry point the fuzzer uses.
    #[test]
    fn rtsp_fuzzer_entry(data in prop::collection::vec(any::<u8>(), 0..2000)) {
        prop_assert_eq!(rtsp_stream_bytes(&data), Ok(()));
    }

    /// Random bytes as an announced SDP.
    #[test]
    fn sdp_random_bytes(data in prop::collection::vec(any::<u8>(), 0..2000)) {
        prop_assert_eq!(sdp_announce(&data), Ok(()));
    }

    /// FFmpeg's announce with lines replaced, dropped or duplicated.
    #[test]
    fn sdp_mutated_announce(
        edits in prop::collection::vec((any::<prop::sample::Index>(), 0u8..3, "[ -~]{0,60}"), 1..6),
    ) {
        let mut lines: Vec<String> = FFMPEG_SDP.split("\r\n").map(str::to_owned).collect();
        for (at, op, text) in edits {
            let i = at.index(lines.len());
            match op {
                0 => lines[i] = text,
                1 => { lines.remove(i); }
                _ => { let line = lines[i].clone(); lines.insert(i, line); }
            }
            if lines.is_empty() {
                break;
            }
        }
        prop_assert_eq!(sdp_announce(lines.join("\r\n").as_bytes()), Ok(()));
    }

    /// Any track list survives `DESCRIBE` then `ANNOUNCE`'s parser.
    #[test]
    fn sdp_describe_reads_back(data in prop::collection::vec(any::<u8>(), 0..80)) {
        prop_assert_eq!(sdp_roundtrip(&tracks_from_bytes(&data)), Ok(()));
    }

    /// Valid RTP headers, payloads shaped like the codec's own, at random
    /// sequence numbers and timestamps, into every depacketizer.
    #[test]
    fn depacketizers_take_plausible_packets(
        which in 0usize..DEPACKETIZERS,
        packets in prop::collection::vec((any::<u16>(), 0u32..4, any::<bool>(), nal_payload()), 1..40),
    ) {
        let mut seq = 0u16;
        let packets: Vec<Vec<u8>> = packets
            .into_iter()
            .map(|(jump, ts, marker, payload)| {
                // Mostly in order, sometimes a gap or a step back.
                seq = seq.wrapping_add(if jump % 8 == 0 { jump } else { 1 });
                rtp(seq, ts * 3000, marker, &payload)
            })
            .collect();
        prop_assert_eq!(depacketize(which, &packets), Ok(()));
    }

    /// The byte-driven entry point the fuzzer uses.
    #[test]
    fn depacketizer_fuzzer_entry(data in prop::collection::vec(any::<u8>(), 0..4000)) {
        prop_assert_eq!(depacketize_bytes(&data), Ok(()));
    }

    /// Every codec the server packetizes, any sizes: what goes in comes out.
    #[test]
    fn packetizers_roundtrip(
        codec in prop::sample::select(Codec::ALL.to_vec()),
        bodies in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3000), 1..5),
    ) {
        prop_assert_eq!(packetize_roundtrip(codec, &bodies), Ok(()));
    }

    /// Random bytes as a whole JPEG file.
    #[test]
    fn jpeg_random_files(data in prop::collection::vec(any::<u8>(), 0..3000)) {
        prop_assert_eq!(jpeg_frame(&[&[0u8][..], &data].concat()), Ok(()));
    }

    /// A real file with bytes replaced, inserted or removed, and cut anywhere:
    /// the reader refuses or accepts it, and what it accepts goes out whole.
    #[test]
    fn jpeg_mutated_files(
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>(), 0u8..3), 1..8),
        cut in prop::option::of(any::<prop::sample::Index>()),
    ) {
        let mut file = crate::fuzzing::jpeg_harness_seed().to_vec();
        for (at, byte, op) in edits {
            let i = at.index(file.len());
            match op {
                0 => file[i] = byte,
                1 => { file.remove(i); }
                _ => file.insert(i, byte),
            }
            if file.is_empty() {
                break;
            }
        }
        if let Some(cut) = cut {
            file.truncate(cut.index(file.len() + 1));
        }
        prop_assert_eq!(jpeg_frame(&[&[0u8][..], &file].concat()), Ok(()));
    }

    /// A real file coded with tables of its own, mutated like the standard one:
    /// whatever the coder takes goes out whole, whatever it does not is dropped.
    #[test]
    fn jpeg_mutated_optimized_files(
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>(), 0u8..3), 1..8),
        cut in prop::option::of(any::<prop::sample::Index>()),
    ) {
        let mut file = crate::fuzzing::jpeg_harness_seed_optimized().to_vec();
        for (at, byte, op) in edits {
            let i = at.index(file.len());
            match op {
                0 => file[i] = byte,
                1 => { file.remove(i); }
                _ => file.insert(i, byte),
            }
            if file.is_empty() {
                break;
            }
        }
        if let Some(cut) = cut {
            file.truncate(cut.index(file.len() + 1));
        }
        prop_assert_eq!(jpeg_frame(&[&[0u8][..], &file].concat()), Ok(()));
    }

    /// Any scan behind valid headers, any size: cut at the budget and put back.
    #[test]
    fn jpeg_scans_behind_valid_headers(mode in 1u8..=255, scan in prop::collection::vec(any::<u8>(), 1..6000)) {
        prop_assert_eq!(jpeg_frame(&[&[mode][..], &scan].concat()), Ok(()));
    }

    /// The byte-driven entry point the fuzzer uses.
    #[test]
    fn jpeg_fuzzer_entry(data in prop::collection::vec(any::<u8>(), 0..3000)) {
        prop_assert_eq!(jpeg_frame(&data), Ok(()));
    }

    /// Random bytes as a Digest header: no panic, within the cap, read back.
    #[test]
    fn authorization_random_bytes(data in prop::collection::vec(any::<u8>(), 0..600)) {
        prop_assert_eq!(authorization(&data), Ok(()));
    }

    /// Headers shaped like a client's, with the characters that matter to the reader.
    #[test]
    fn authorization_plausible_headers(
        pairs in prop::collection::vec(("[a-z]{1,8}", "[ -~]{0,20}", any::<bool>()), 0..20),
    ) {
        let text: Vec<String> = pairs
            .iter()
            .map(|(k, v, quoted)| {
                if *quoted {
                    format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        prop_assert_eq!(authorization(text.join(", ").as_bytes()), Ok(()));
    }
}
