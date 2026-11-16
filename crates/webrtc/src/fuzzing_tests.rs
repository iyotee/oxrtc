//! The fuzz targets' harnesses on stable, under proptest: random bytes, and
//! inputs close enough to valid ones to reach past the first check.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more for a soak run with
/// `OXRTC_PROPTEST_CASES=200000`.
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// A STUN header (any class and method of the TURN set) and attributes of
/// the kinds this client reads, with lengths that may lie.
fn stunlike() -> impl Strategy<Value = Vec<u8>> {
    let kinds = prop::sample::select(vec![
        attr::XOR_MAPPED_ADDRESS,
        attr::XOR_RELAYED_ADDRESS,
        attr::XOR_PEER_ADDRESS,
        attr::LIFETIME,
        attr::DATA,
        attr::CHANNEL_NUMBER,
        attr::ERROR_CODE,
        attr::REALM,
        attr::NONCE,
        attr::MESSAGE_INTEGRITY,
        attr::FINGERPRINT,
        0x7777,
    ]);
    let attribute = (kinds, prop::collection::vec(any::<u8>(), 0..40), prop::option::of(any::<u16>()));
    (
        prop::sample::select(vec![0x0001u16, 0x0101, 0x0103, 0x0113, 0x0104, 0x0108, 0x0109, 0x0017, 0x0119]),
        any::<[u8; 12]>(),
        prop::collection::vec(attribute, 0..12),
        any::<bool>(),
    )
        .prop_map(|(kind, id, attrs, lie)| {
            let mut out = kind.to_be_bytes().to_vec();
            out.extend_from_slice(&[0, 0, 0x21, 0x12, 0xA4, 0x42]);
            out.extend_from_slice(&id);
            for (t, value, length) in attrs {
                out.extend_from_slice(&t.to_be_bytes());
                let declared = length.filter(|_| lie).unwrap_or(value.len() as u16);
                out.extend_from_slice(&declared.to_be_bytes());
                out.extend_from_slice(&value);
                out.resize(out.len().div_ceil(4) * 4, 0);
            }
            let length = (out.len() - 20) as u16;
            out[2..4].copy_from_slice(&length.to_be_bytes());
            out
        })
}

/// A harness script of steps (see [`turn_datagrams`]) whose datagrams are
/// STUN-like.
fn script() -> impl Strategy<Value = Vec<u8>> {
    let step = prop_oneof![
        (0u8..2, stunlike()).prop_map(|(mode, datagram)| {
            let len = datagram.len().min(1023);
            let mut step = vec![mode, (len / 4) as u8, (len % 4) as u8];
            step.extend_from_slice(&datagram[..len]);
            step
        }),
        prop::collection::vec(any::<u8>(), 1..8).prop_map(|mut rest| {
            rest.insert(0, 2);
            rest
        }),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| vec![3, a, b]),
        any::<u8>().prop_map(|t| vec![4, t]),
    ];
    (any::<u8>(), prop::collection::vec(step, 0..24)).prop_map(|(first, steps)| {
        let mut out = vec![first];
        steps.into_iter().for_each(|s| out.extend(s));
        out
    })
}

/// A TURN stream: a mode byte and messages back to back, STUN-like ones and
/// padded ChannelData, the last one possibly cut short or followed by junk.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    let channel = (0x4000u16..0x5000, prop::collection::vec(any::<u8>(), 0..70)).prop_map(|(number, data)| {
        let mut out = number.to_be_bytes().to_vec();
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
        out.resize(out.len().div_ceil(4) * 4, 0);
        out
    });
    // Longer than the framer holds: read past once the server has spoken.
    let long = (0x4000u16..0x5000, 16_381usize..20_000).prop_map(|(number, size)| {
        let mut out = number.to_be_bytes().to_vec();
        out.extend_from_slice(&(size as u16).to_be_bytes());
        out.resize((4 + size).div_ceil(4) * 4, 0);
        out
    });
    let message = prop_oneof![8 => stunlike(), 8 => channel, 1 => long];
    (any::<u8>(), prop::collection::vec(message, 0..8), prop::collection::vec(any::<u8>(), 0..8), 0usize..40).prop_map(
        |(mode, messages, junk, cut)| {
            let mut out = vec![mode];
            messages.into_iter().for_each(|m| out.extend(m));
            out.truncate(out.len().saturating_sub(cut % 3 * 5));
            out.extend(junk.into_iter().take(cut % 4));
            out
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn any_bytes_through_the_stun_parser(data in prop::collection::vec(any::<u8>(), 0..600)) {
        prop_assert_eq!(stun_datagram(&data), Ok(()));
    }

    #[test]
    fn stunlike_datagrams_through_the_parser(data in stunlike()) {
        prop_assert_eq!(stun_datagram(&data), Ok(()));
    }

    #[test]
    fn any_script_through_an_allocation(data in prop::collection::vec(any::<u8>(), 0..800)) {
        prop_assert_eq!(turn_datagrams(&data), Ok(()));
    }

    #[test]
    fn stunlike_scripts_through_an_allocation(data in script()) {
        prop_assert_eq!(turn_datagrams(&data), Ok(()));
    }

    #[test]
    fn any_bytes_as_a_turn_stream(data in prop::collection::vec(any::<u8>(), 0..3000)) {
        prop_assert_eq!(turn_stream(&data), Ok(()));
    }

    #[test]
    fn turn_messages_back_to_back_as_a_stream(data in stream()) {
        prop_assert_eq!(turn_stream(&data), Ok(()));
    }

    #[test]
    fn ice_urls_print_as_they_parse(text in "(stun|turn|turns|TURN):(\\[[0-9a-f:]{2,12}\\]|[a-z0-9.-]{1,20}|[0-9.]{7,15})(:[0-9]{1,6})?(\\?transport=(udp|tcp|x))?") {
        prop_assert_eq!(ice_url(text.as_bytes()), Ok(()));
    }

    #[test]
    fn any_text_as_an_ice_url(data in prop::collection::vec(any::<u8>(), 0..80)) {
        prop_assert_eq!(ice_url(&data), Ok(()));
    }
}

/// Every seed of `fuzz/seeds/<target>/` through its harness.
#[test]
fn fuzz_seeds_replay() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    type Harness = fn(&[u8]) -> Result<(), String>;
    let harnesses: [(&str, Harness); 3] =
        [("turn_datagrams", turn_datagrams), ("turn_stream", turn_stream), ("ice_url", ice_url)];
    let mut replayed = 0;
    for (target, harness) in harnesses {
        for entry in std::fs::read_dir(seeds.join(target)).unwrap() {
            let path = entry.unwrap().path();
            let data = std::fs::read(&path).unwrap();
            assert_eq!(harness(&data), Ok(()), "{}", path.display());
            replayed += 1;
        }
    }
    assert!(replayed >= 8, "only {replayed} seeds found under {}", seeds.display());
}

/// The relaying seed reaches the relay: its Data indication is delivered.
#[test]
fn the_data_seed_is_relayed() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    let seed = std::fs::read(seeds.join("turn_datagrams/data_indication")).unwrap();
    let mut allocation = allocation(Instant::now(), true, false);
    let datagram = &seed[4..];
    assert!(matches!(allocation.handle(Instant::now(), datagram), Incoming::Relayed { data: b"hello", .. }));
}
